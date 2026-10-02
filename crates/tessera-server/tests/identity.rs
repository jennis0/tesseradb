//! Principals, credentials and sessions, through the three listeners: what a session holds, which
//! catalogue changes end which sessions, OIDC access tokens, key expiry, and the catalogue across
//! a restart.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    control(
        server,
        post.clone(),
        "/control/grants",
        json!({ "principal": name, "terms": terms }),
    )
    .await;
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

/// A principal with `write` writes against the whole corpus. Flushing needs `write-all` as well,
/// and `admin` is not needed for either.
#[tokio::test]
async fn a_write_needs_write_and_a_flush_needs_write_all() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let post = reqwest::Method::POST;
    control(&server, post.clone(), "/control/principals", json!({ "name": "pipeline", "kind": "service" })).await;
    control(&server, post.clone(), "/control/grants", json!({ "principal": "pipeline", "permission": "write" })).await;
    let key = control(&server, post.clone(), "/control/principals/pipeline/keys", json!({})).await;
    let key = key["key"].as_str().unwrap().to_owned();
    let suppress = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(&key)
        .json(&json!([{ "op": "suppress", "match": { "id": member(3) } }]))
        .send()
        .await
        .unwrap();
    assert_eq!(suppress.status(), 200);
    let flush = || {
        server
            .client
            .post(server.control_url("/control/flush?wait=visible"))
            .bearer_auth(&key)
            .send()
    };
    assert_eq!(flush().await.unwrap().status(), 403);
    control(&server, post, "/control/grants", json!({ "principal": "pipeline", "permission": "write-all" })).await;
    assert_eq!(flush().await.unwrap().status(), 202);
}

/// A session of a principal holding `read-all` reads every item through the masked path, so a
/// suppression still applies to it. A session minted for that principal through `authorise-as`
/// carries only its `read` and `write`, and reads only its terms.
#[tokio::test]
async fn read_all_reads_every_item_and_never_reaches_a_minted_session() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let post = reqwest::Method::POST;
    person(&server, "auditor", &["1"]).await;
    for p in ["write", "read-all", "write-all"] {
        control(&server, post.clone(), "/control/grants", json!({ "principal": "auditor", "permission": p })).await;
    }
    let own = login_password(&server, "auditor").await;
    assert_eq!(visible(&server, &own).await, N_ITEMS);
    let minted = authorise_as(&server, &server.integrator_key, "auditor").await;
    assert_eq!(visible(&server, &minted).await, N_ITEMS.div_ceil(3));

    let listed = control(&server, reqwest::Method::GET, "/control/sessions?principal=auditor", json!({})).await;
    let mut carried: Vec<(Option<String>, Value)> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["minted_by"].as_str().map(str::to_owned), s["permissions"].clone()))
        .collect();
    carried.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        carried,
        vec![
            (None, json!(["read", "write", "read-all", "write-all"])),
            (Some(INTEGRATOR.to_owned()), json!(["read", "write"])),
        ]
    );

    // The operator's own session reads every item too, carrying `read` and `read-all` alone, and
    // a suppression applies to both.
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "read_all": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let answer = resp.json::<Value>().await.unwrap();
    let operator = answer["token"].as_str().unwrap().to_owned();
    assert_eq!(visible(&server, &operator).await, N_ITEMS);
    let listed = control(&server, reqwest::Method::GET, "/control/sessions", json!({})).await;
    let session = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["token_id"] == answer["token_id"])
        .unwrap();
    assert_eq!(session["permissions"], json!(["read", "read-all"]));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([{ "op": "suppress", "match": { "id": member(3) } }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(visible(&server, &operator).await, N_ITEMS - 1);
    assert_eq!(visible(&server, &own).await, N_ITEMS - 1);

    // Only the operator credential asks for a session reading every item.
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&json!({ "read_all": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

/// Logout ends the session it is sent with, and only that one.
#[tokio::test]
async fn logout_ends_the_session_it_is_sent_with() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    person(&server, "ann", &["0"]).await;
    let first = login_password(&server, "ann").await;
    let second = login_password(&server, "ann").await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/logout"))
        .bearer_auth(&first)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(meta_status(&server, &first).await, 403);
    assert_eq!(meta_status(&server, &second).await, 200);
}

/// An integrator revokes the sessions it minted, and an attempt on another integrator's session
/// is answered the same way and ends nothing.
#[tokio::test]
async fn an_integrator_cannot_revoke_a_session_another_minted() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let post = reqwest::Method::POST;
    person(&server, "ann", &["0"]).await;
    control(&server, post.clone(), "/control/principals", json!({ "name": "partner", "kind": "service" })).await;
    control(&server, post.clone(), "/control/grants", json!({ "principal": "partner", "permission": "authorise-as" })).await;
    let partner = control(&server, post.clone(), "/control/principals/partner/keys", json!({})).await;
    let partner = partner["key"].as_str().unwrap().to_owned();

    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&json!({ "principal": "ann" }))
        .send()
        .await
        .unwrap();
    let minted: Value = resp.json().await.unwrap();
    let token = minted["token"].as_str().unwrap().to_owned();
    let revoke = |key: String| {
        let server = &server;
        let token_id = minted["token_id"].clone();
        async move {
            server
                .client
                .post(server.session_url("/session/revoke"))
                .bearer_auth(key)
                .json(&json!({ "token_id": token_id }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(revoke(partner).await, 204);
    assert_eq!(meta_status(&server, &token).await, 200);
    assert_eq!(revoke(server.integrator_key.clone()).await, 204);
    assert_eq!(meta_status(&server, &token).await, 403);
}

/// A change whose client goes away before the answer still ends the sessions it affected.
#[tokio::test]
async fn a_committed_change_ends_its_sessions_when_its_client_goes_away() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    person(&server, "ann", &["0"]).await;
    let token = login_password(&server, "ann").await;

    // Setting a password hashes it with argon2id first, which outlasts the client's patience.
    let impatient = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(20))
        .build()
        .unwrap();
    let gone = impatient
        .put(server.control_url("/control/principals/ann/password"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "password": "a different long passphrase" }))
        .send()
        .await;
    assert!(gone.is_err(), "the client gave up before the answer");

    // Wait for the change to commit, seen as the old password no longer logging in, then for the
    // committing thread to end the session, which it does straight after.
    let wait = |what: &'static str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        move || assert!(std::time::Instant::now() < deadline, "{what}")
    };
    let check = wait("the change never committed");
    while login(&server, json!({ "password": { "principal": "ann", "password": PASSWORD } }))
        .await
        .status()
        == 200
    {
        check();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let check = wait("the change committed and its session was not ended");
    while meta_status(&server, &token).await == 200 {
        check();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(meta_status(&server, &token).await, 403);
}

/// A test identity provider: an Ed25519 key published at a loopback JWKS URL, which counts the
/// fetches it answers and can be taken down.
struct Idp {
    url: String,
    keys: Arc<parking_lot::Mutex<Value>>,
    signers: Vec<(String, Vec<u8>, Value)>,
    fetches: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

impl Idp {
    async fn start() -> Idp {
        let keys = Arc::new(parking_lot::Mutex::new(json!({ "keys": [] })));
        let fetches = Arc::new(AtomicUsize::new(0));
        let down = Arc::new(AtomicBool::new(false));
        let (served, counted, failing) = (Arc::clone(&keys), Arc::clone(&fetches), Arc::clone(&down));
        let app = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || {
                let served = Arc::clone(&served);
                let counted = Arc::clone(&counted);
                let failing = Arc::clone(&failing);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Slow enough that concurrent verifications overlap the fetch.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if failing.load(Ordering::SeqCst) {
                        return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                    }
                    Ok(axum::Json(served.lock().clone()))
                }
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
            fetches,
            down,
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
        self.typed_token(kid, Some("JWT"), claims)
    }

    fn typed_token(&self, kid: &str, typ: Option<&str>, claims: Value) -> String {
        let (_, der, _) = self.signers.iter().find(|(k, _, _)| k == kid).unwrap();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_owned());
        header.typ = typ.map(str::to_owned);
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(der)).unwrap()
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

/// A token with `alg: none` and no signature, which no verifier may accept.
fn unsigned(claims: &Value) -> String {
    let part = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.", part(&json!({ "alg": "none", "typ": "JWT" })), part(claims))
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
    for (group, permissions) in [
        ("readers", &["read"][..]),
        ("admins", &["read", "admin"][..]),
        ("auditors", &["read", "read-all"][..]),
    ] {
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
            { "claim": "groups[*]", "value": "tessera-auditors", "group": "auditors" },
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
    let mut not_yet = claims(&["tessera-readers"], exp);
    not_yet["nbf"] = json!(now_secs() + 120);
    let mut not_yet_as_text = claims(&["tessera-readers"], exp);
    not_yet_as_text["nbf"] = json!((now_secs() + 120).to_string());
    let mut expiry_as_text = claims(&["tessera-readers"], exp);
    expiry_as_text["exp"] = json!(exp.to_string());
    let mut wrong_issuer = claims(&["tessera-readers"], exp);
    wrong_issuer["iss"] = json!("https://login.example.net");
    idp.add_key("unpublished");
    for (why, bad) in [
        ("wrong audience", idp.token("k1", wrong_audience)),
        ("no subject", idp.token("k1", no_subject)),
        ("expired", idp.token("k1", claims(&["tessera-readers"], now_secs() - 1))),
        ("HS256", hs256),
        ("not before", idp.token("k1", not_yet)),
        ("not before, as a string", idp.token("k1", not_yet_as_text)),
        ("expiry as a string", idp.token("k1", expiry_as_text)),
        ("wrong issuer", idp.token("k1", wrong_issuer)),
        ("unknown key", idp.token("unpublished", claims(&["tessera-readers"], exp))),
        ("alg none", unsigned(&claims(&["tessera-readers"], exp))),
        (
            "a logout token",
            idp.typed_token("k1", Some("logout+jwt"), claims(&["tessera-readers"], exp)),
        ),
    ] {
        let resp = login(&server, json!({ "access_token": bad })).await;
        assert_eq!(resp.status(), 401, "{why}");
    }
    // A token typed as an access token, or untyped, is accepted.
    for typ in [Some("at+jwt"), Some("application/at+jwt"), None] {
        let token = idp.typed_token("k1", typ, claims(&["tessera-readers"], exp));
        let resp = login(&server, json!({ "access_token": token })).await;
        assert_eq!(resp.status(), 200, "{typ:?}");
    }

    // A mapping to a group holding `read-all` reads every item, and a session minted through
    // `authorise-as` for the same identity reads only its terms.
    let auditor = idp.token("k1", claims(&["tessera-auditors"], exp));
    let resp = login(&server, json!({ "access_token": auditor.clone() })).await;
    let own = resp.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned();
    assert_eq!(visible(&server, &own).await, N_ITEMS);
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&json!({ "access_token": auditor }))
        .send()
        .await
        .unwrap();
    let minted = resp.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned();
    assert_eq!(visible(&server, &minted).await, 0);

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
            password_gate: generous_password_gate(),
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

/// A catalogue declaring `corp` with `idp`'s keys, and a verifier with the intervals given and a
/// ceiling of a day.
fn verifier_for(
    idp: &Idp,
    refetch: std::time::Duration,
    max_age: std::time::Duration,
) -> (tessera_catalogue::Catalogue, tessera_server::oidc::Verifier, TempDir) {
    verifier_with_ceiling(idp, refetch, max_age, std::time::Duration::from_secs(24 * 3600))
}

/// A catalogue declaring `corp` with `idp`'s keys, and a verifier with the intervals given.
fn verifier_with_ceiling(
    idp: &Idp,
    refetch: std::time::Duration,
    max_age: std::time::Duration,
    ceiling: std::time::Duration,
) -> (tessera_catalogue::Catalogue, tessera_server::oidc::Verifier, TempDir) {
    let (catalogue, _, dir) = test_identity();
    catalogue
        .create_provider(&tessera_catalogue::Provider {
            name: "corp".into(),
            issuer: ISSUER.into(),
            audience: "tessera".into(),
            jwks_url: idp.url.clone(),
            rules: Vec::new(),
            role_mappings: Vec::new(),
        })
        .unwrap();
    let verifier = tessera_server::oidc::Verifier::with_intervals(refetch, max_age, ceiling);
    (catalogue, verifier, dir)
}

/// Concurrent tokens naming a key the provider does not publish cost one fetch, and a failed
/// fetch is not repeated within the refetch interval.
#[tokio::test(flavor = "multi_thread")]
async fn a_jwks_url_is_fetched_once_at_a_time_and_a_failure_is_remembered() {
    let mut idp = Idp::start().await;
    let hour = std::time::Duration::from_secs(3600);
    let (catalogue, verifier, _dir) = verifier_for(&idp, hour, hour);
    idp.add_key("unpublished");
    let exp = now_secs() + 300;
    let unknown = idp.token("unpublished", claims(&[], exp));
    let (catalogue, verifier) = (Arc::new(catalogue), Arc::new(verifier));
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (catalogue, verifier, unknown) = (Arc::clone(&catalogue), Arc::clone(&verifier), unknown.clone());
        attempts.spawn(async move { verifier.verify(&catalogue, &unknown).await.is_none() });
    }
    assert!(attempts.join_all().await.into_iter().all(|refused| refused));
    assert_eq!(idp.fetches(), 1);
    assert!(verifier.verify(&catalogue, &idp.token("k1", claims(&[], exp))).await.is_some());
    assert_eq!(idp.fetches(), 1);

    idp.down.store(true, Ordering::SeqCst);
    let (catalogue, verifier, _dir) = verifier_for(&idp, hour, hour);
    let token = idp.token("k1", claims(&[], exp));
    assert!(verifier.verify(&catalogue, &token).await.is_none());
    assert!(verifier.verify(&catalogue, &token).await.is_none());
    assert_eq!(idp.fetches(), 2);
}

/// Keys past their age are fetched again, and kept in use when that fetch fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_refetch_keeps_the_keys_held() {
    let idp = Idp::start().await;
    let zero = std::time::Duration::ZERO;
    let (catalogue, verifier, _dir) = verifier_for(&idp, zero, zero);
    let token = idp.token("k1", claims(&[], now_secs() + 300));
    assert!(verifier.verify(&catalogue, &token).await.is_some());
    idp.down.store(true, Ordering::SeqCst);
    assert!(verifier.verify(&catalogue, &token).await.is_some());
    assert_eq!(idp.fetches(), 2);
}

/// A fetch whose caller goes away completes and stores the keys it found, so the next token is
/// verified without waiting out the refetch interval.
#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_completes_when_its_caller_goes_away() {
    let idp = Idp::start().await;
    let hour = std::time::Duration::from_secs(3600);
    let (catalogue, verifier, _dir) = verifier_for(&idp, hour, hour);
    let token = idp.token("k1", claims(&[], now_secs() + 300));
    let short = std::time::Duration::from_millis(10);
    let gone = tokio::time::timeout(short, verifier.verify(&catalogue, &token)).await;
    assert!(gone.is_err(), "the caller went away before the fetch ended");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(verifier.verify(&catalogue, &token).await.is_some());
    assert_eq!(idp.fetches(), 1);
}

/// Keys kept after a failed refetch are used until they reach the ceiling, and then the
/// provider's tokens are refused until a fetch succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn keys_past_the_ceiling_are_not_used() {
    let idp = Idp::start().await;
    let zero = std::time::Duration::ZERO;
    let ceiling = std::time::Duration::from_millis(500);
    let (catalogue, verifier, _dir) = verifier_with_ceiling(&idp, zero, zero, ceiling);
    let token = idp.token("k1", claims(&[], now_secs() + 300));
    assert!(verifier.verify(&catalogue, &token).await.is_some());
    idp.down.store(true, Ordering::SeqCst);
    assert!(verifier.verify(&catalogue, &token).await.is_some());
    tokio::time::sleep(ceiling).await;
    assert!(verifier.verify(&catalogue, &token).await.is_none());
    idp.down.store(false, Ordering::SeqCst);
    assert!(verifier.verify(&catalogue, &token).await.is_some());
}

/// A token that two providers with the same issuer each accept is refused, and a token only one
/// of them accepts logs in.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_two_providers_accept_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let idp = Idp::start().await;
    let post = reqwest::Method::POST;
    control(&server, post.clone(), "/control/groups", json!({ "name": "readers" })).await;
    control(&server, post, "/control/grants", json!({ "group": "readers", "permission": "read" })).await;
    for (name, audience) in [("corp", "tessera"), ("partner", "tessera-partner")] {
        let provider = json!({
            "issuer": ISSUER,
            "audience": audience,
            "jwks_url": idp.url,
            "role_mappings": [{ "claim": "groups[*]", "value": "tessera-readers", "group": "readers" }],
        });
        control(&server, reqwest::Method::PUT, &format!("/control/providers/{name}"), provider).await;
    }
    let exp = now_secs() + 300;
    let mut both = claims(&["tessera-readers"], exp);
    both["aud"] = json!(["tessera", "tessera-partner"]);
    let resp = login(&server, json!({ "access_token": idp.token("k1", both) })).await;
    assert_eq!(resp.status(), 401);
    let one = claims(&["tessera-readers"], exp);
    let resp = login(&server, json!({ "access_token": idp.token("k1", one) })).await;
    assert_eq!(resp.status(), 200);
}
