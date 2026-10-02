//! **The OpenAPI description is kept true here, or it is not true.**
//!
//! `docs/openapi/tessera.yaml` is hand-authored: the JSON DTOs live in this crate, some private,
//! the control plane's answers are built with `json!`, and the wire structs carry no serde derive,
//! so nothing generates it from the types and nothing but this test stops it drifting. Every route
//! the viewer, session and control planes mount is exercised, with a success and at least one
//! refusal each, and **every JSON request and response body is validated against the
//! description's own schemas**. The fault-injection routes a test build adds to the control plane
//! are not described. The closed DTOs are declared `additionalProperties: false`, so a field added
//! to a response and not to the document fails here rather than being discovered by a stranger
//! reading the wire.
//!
//! What this test does not do: decode the framed-Arrow body of `/v1/viewport` against a schema —
//! there is none to write, and the frame layout is contracts §5, checked by
//! [`common::decode_viewport_frames`] and by the two worked decodes under `clients/ts/wire-example`
//! and `reference/examples`. It asserts the route's headers and its framing outcomes (a `k = 0`
//! request yields no points frame; `layers: []` yields no artifacts frame) and validates the
//! request body only. `POST /v1/items`, `POST /v1/artifacts` and `POST /v1/aggregate` are framed
//! the same way, and their JSON frames are validated against their schemas.

mod common;

use std::path::Path;
use std::sync::OnceLock;

use arrow::array::{Float32Array, StringArray};
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_server::state::ComputeGate;

// ---------------------------------------------------------------------------------------------
// The description, and validators over its component schemas.
// ---------------------------------------------------------------------------------------------

const DESCRIPTION: &str = include_str!("../../../docs/openapi/tessera.yaml");

fn description() -> Value {
    let doc: Value = serde_yaml_ng::from_str(DESCRIPTION).expect("tessera.yaml parses as YAML");
    assert_eq!(doc["openapi"], "3.1.0", "the description is OpenAPI 3.1");
    doc
}

/// A validator for one named schema under `components/schemas`. The whole `components` block
/// rides along as the root document so `$ref: "#/components/schemas/…"` resolves exactly as it
/// does in the file — a schema is validated in its own context, never copied out of it.
fn validator(doc: &Value, schema: &str) -> jsonschema::Validator {
    let root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/components/schemas/{schema}"),
        "components": doc["components"].clone(),
    });
    jsonschema::draft202012::new(&root)
        .unwrap_or_else(|e| panic!("schema {schema} does not compile: {e}"))
}

fn assert_valid(doc: &Value, schema: &str, instance: &Value) {
    let v = validator(doc, schema);
    let errors: Vec<String> = v.iter_errors(instance).map(|e| e.to_string()).collect();
    assert!(
        errors.is_empty(),
        "{schema} rejects {instance}:\n  {}",
        errors.join("\n  ")
    );
}

fn assert_invalid(doc: &Value, schema: &str, instance: &Value) {
    assert!(
        !validator(doc, schema).is_valid(instance),
        "{schema} accepted {instance}, which it must not"
    );
}

/// The error envelope on a refusal: a status the description declares for the route, one closed
/// code, and — on a 429 — `Retry-After` agreeing with the body. Returns the body for further
/// asserts.
async fn assert_refusal(doc: &Value, resp: reqwest::Response, status: u16, code: &str) -> Value {
    assert_refusal_to(doc, None, resp, status, code).await
}

/// [`assert_refusal`] for the operation `method` names, which a path with several operations
/// needs.
async fn assert_refusal_to(
    doc: &Value,
    method: Option<&reqwest::Method>,
    resp: reqwest::Response,
    status: u16,
    code: &str,
) -> Value {
    let path = resp.url().path().to_string();
    let got = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "{path} answered {got}: {text}");
    let responses = described_responses(doc, method, &path);
    assert!(
        responses.get(status.to_string()).is_some(),
        "{path} answered {status}, which the description does not declare for it"
    );
    let body: Value = serde_json::from_str(&text).expect("a refusal carries the JSON envelope");
    assert_valid(doc, "Error", &body);
    assert_eq!(body["error"], code, "{body}");
    if status == 429 {
        assert_valid(doc, "BackpressureError", &body);
        let header = retry_after.expect("every 429 carries Retry-After");
        assert_eq!(
            header.parse::<u64>().unwrap(),
            body["retry_after_s"].as_u64().unwrap(),
            "Retry-After and retry_after_s must agree"
        );
    } else {
        assert!(retry_after.is_none(), "only a 429 carries Retry-After");
        assert!(body.get("retry_after_s").is_none());
    }
    body
}

/// The `responses` the description declares for the route `path` was sent to, under `method`.
/// A literal segment outranks a parameter, so `/v1/artifacts/browse` is not read as
/// `/v1/artifacts/{tessera_id}`. With no `method`, the path must have one operation.
fn described_responses<'a>(
    doc: &'a Value,
    method: Option<&reqwest::Method>,
    path: &str,
) -> &'a Value {
    let sent: Vec<&str> = path.split('/').collect();
    let (_, item) = doc["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(template, item)| {
            let described: Vec<&str> = template.split('/').collect();
            if described.len() != sent.len() {
                return None;
            }
            let mut literal = 0;
            for (d, s) in described.iter().zip(&sent) {
                if d == s {
                    literal += 1;
                } else if !d.starts_with('{') {
                    return None;
                }
            }
            Some((literal, item))
        })
        .max_by_key(|(literal, _)| *literal)
        .unwrap_or_else(|| panic!("{path} is not a described route"));
    let operations = item.as_object().unwrap();
    let operation = match method {
        Some(method) => operations
            .get(&method.as_str().to_ascii_lowercase())
            .unwrap_or_else(|| panic!("{method} {path} is not a described operation")),
        None => {
            assert_eq!(
                operations.len(),
                1,
                "{path} has several operations; name the method"
            );
            operations.values().next().unwrap()
        }
    };
    &operation["responses"]
}

/// A control-plane answer: a status the description declares for `method` and the route, and a
/// JSON body valid against the schema it names for that status. Returns the body.
async fn assert_answer(
    doc: &Value,
    method: &reqwest::Method,
    resp: reqwest::Response,
    status: u16,
) -> Value {
    let path = resp.url().path().to_string();
    let got = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "{method} {path} answered {got}: {text}");
    let described = &described_responses(doc, Some(method), &path)[status.to_string()];
    let schema = described["content"]["application/json"]["schema"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("{method} {path} declares no JSON body for {status}"));
    let body: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{method} {path}'s {status} is not JSON ({e}): {text}"));
    assert_valid(
        doc,
        schema.trim_start_matches("#/components/schemas/"),
        &body,
    );
    body
}

/// A refusal the HTTP framework makes before a handler runs: a status the description declares
/// for `method` and the route, with a plain-text body rather than the envelope.
async fn assert_framework_refusal(
    doc: &Value,
    method: &reqwest::Method,
    resp: reqwest::Response,
    status: u16,
) {
    let path = resp.url().path().to_string();
    let got = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "{method} {path} answered {got}: {text}");
    assert!(
        described_responses(doc, Some(method), &path)
            .get(status.to_string())
            .is_some(),
        "{method} {path} answered {status}, which the description does not declare for it"
    );
    assert!(
        content_type.starts_with("text/plain"),
        "{method} {path}'s {status} is described as plain text and came as {content_type:?}: {text}"
    );
}

// ---------------------------------------------------------------------------------------------
// The fixture: a category column so `/v1/categories` has something to serve, a rendered number,
// and — registered over the control plane once the server is up — a layer holding artifacts so
// `/v1/artifacts` and the artifacts frame have something to answer with.
// ---------------------------------------------------------------------------------------------

const N: u64 = 64;

const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  astro = 11
  cond = 22
  hep = 33

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"

[[attribute]]
name     = "score"
type     = "f32"
render   = true

[[attribute]]
name     = "id"
type     = "u64"
unique   = true
field    = "entity_id"
"#;

fn build_bundle(dir: &Path) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    let archive = StringArray::from_iter_values(
        ids.iter()
            .map(|&e| ["astro", "cond", "hep"][(e % 3) as usize]),
    );
    let score = Float32Array::from_iter(ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)));
    write_points(
        &points,
        &ids,
        scatter,
        vec![
            column("archive", false, archive),
            column("score", true, score),
        ],
    );
    write_pairs_n(&pairs, N);
    build_declared(&dir.join("bundle"), &points, &pairs, SCHEMA_TOML);
}

/// A copy of [`build_bundle`]'s bundle in `tmp`, built once for this binary.
fn copy_bundle(tmp: &TempDir) -> std::path::PathBuf {
    static BUILT: OnceLock<TempDir> = OnceLock::new();
    copy_built(&BUILT, tmp.path(), build_bundle)
}

const LAYER: &str = "clusters/a";

/// A flat, enumerated layer over the fixture's `id`s, holding two artifacts: one every
/// principal clears, one only the broad principal does (`terms_of` grants term `1` on
/// `source_id % 3 == 0`). Registered and published over the control plane, so the test brings up
/// the server exactly as a deployment would and adds nothing to the build.
async fn publish_layer(server: &TestServer) -> Vec<String> {
    let declaration = json!({
        "name": LAYER,
        "title": "clusters (a)",
        "views": ["s0"],
        "membership": "enumerated",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": { "count": 1 },
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": ["centroid", "hull", "box"], "supplied": [] },
        "depends_on": [],
        "levels": []
    });
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "{}",
        resp.text().await.unwrap()
    );

    let members_all = members(0..12u64);
    let members_broad = members((12..24u64).filter(|s| s % 3 != 0));
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            LAYER.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "artifacts": [
                { "key": "c0", "members": members_all },
                { "key": "c1", "members": members_broad },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["tessera_id"].as_str().unwrap().to_string())
        .collect()
}

struct Fixture {
    _tmp: TempDir,
    server: TestServer,
    /// The two published artifacts' identifiers, `c0` then `c1`.
    artifacts: Vec<String>,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    copy_bundle(&tmp);
    let server = open(&tmp).await;
    let artifacts = publish_layer(&server).await;
    Fixture {
        _tmp: tmp,
        server,
        artifacts,
    }
}

/// Authorise with the request body validated against the description, returning the response
/// body already validated too.
async fn authorise_checked(doc: &Value, server: &TestServer, terms: &[&str]) -> Value {
    let body = json!({ "principal": principal_for(server, terms) });
    assert_valid(doc, "AuthoriseRequest", &body);
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let out: Value = resp.json().await.unwrap();
    assert_valid(doc, "AuthoriseResponse", &out);
    out
}

async fn viewport(server: &TestServer, token: &str, body: &Value) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap()
}

fn viewport_body(extra: Value) -> Value {
    let mut body = json!({ "view": "s0", "zoom": 1, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200 });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

// ---------------------------------------------------------------------------------------------
// The document itself: every mounted route and nothing else, and every closed DTO closed.
// ---------------------------------------------------------------------------------------------

/// The route set is `viewer::router`, `session::router` and `control::router` without its
/// fault-injection routes, by hand: a route mounted and not described, or described and not
/// mounted, fails here.
#[test]
fn the_description_names_every_route_on_the_three_planes_and_no_other() {
    let doc = description();
    let mut paths: Vec<&str> = doc["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "/control/attributes",
            "/control/changes",
            "/control/compact",
            "/control/flush",
            "/control/grants",
            "/control/grants/revoke",
            "/control/groups",
            "/control/groups/{name}",
            "/control/groups/{name}/members/{principal}",
            "/control/ingest",
            "/control/keys/{prefix}",
            "/control/layers",
            "/control/layers/{name}",
            "/control/layers/{name}/artifacts",
            "/control/principals",
            "/control/principals/{name}",
            "/control/principals/{name}/keys",
            "/control/principals/{name}/password",
            "/control/providers",
            "/control/providers/{name}",
            "/control/sessions",
            "/control/sessions/end",
            "/control/status",
            "/control/view_groups/{name}",
            "/control/views/{group}/{key}",
            "/control/views/{name}",
            "/control/vocabularies/{name}",
            "/control/vocabularies/{name}/values",
            "/healthz",
            "/readyz",
            "/session/authorise",
            "/session/revoke",
            "/v1/aggregate",
            "/v1/artifacts",
            "/v1/artifacts/browse",
            "/v1/artifacts/{tessera_id}",
            "/v1/categories/{column}",
            "/v1/categories/{column}/suggest",
            "/v1/items",
            "/v1/items/{tessera_id}",
            "/v1/login",
            "/v1/logout",
            "/v1/meta",
            "/v1/viewport",
        ]
    );
}

/// The DTOs this crate serialises with a fixed field set are declared closed, so that the
/// validation below fails when a field arrives that the document does not name. Loosening one of
/// these in the file is a change to what this test can catch, and is caught in its own right.
#[test]
fn every_closed_dto_is_declared_closed() {
    let doc = description();
    for name in [
        "Error",
        "AuthoriseRequest",
        "AuthoriseResponse",
        "RevokeRequest",
        "LoginRequest",
        "LoginResponse",
        "CatalogueChange",
        "PrincipalCreate",
        "PrincipalChange",
        "PrincipalRecord",
        "PrincipalList",
        "PasswordSet",
        "KeyCreate",
        "IssuedKey",
        "KeyRecord",
        "KeyList",
        "GroupCreate",
        "GroupRecord",
        "GroupList",
        "Grant",
        "ClaimRule",
        "RoleMapping",
        "ProviderDeclaration",
        "ProviderRecord",
        "ProviderList",
        "SessionRecord",
        "SessionList",
        "SessionsEnd",
        "Meta",
        "DeclaredScalar",
        "Selection",
        "Layer",
        "CategoriesResponse",
        "SuggestRequest",
        "Pin",
        "ViewportRequest",
        "ItemsRequest",
        "ItemsHead",
        "ArtifactsRequest",
        "ArtifactsHead",
        "AggregateRequest",
        "Grouping",
        "AggregateTableHead",
        "PageEnd",
        "RecordsTrailer",
        "ItemRequest",
        "ItemResponse",
        "ArtifactRequest",
        "ArtifactResponse",
        "BrowseRequest",
        "PublicationAck",
        "IngestResponse",
        "ChangeItem",
        "AttributeDeclaration",
        "GroupScope",
        "AttributeDeclared",
        "VocabularyDeclaration",
        "VocabularyValue",
        "VocabularyValues",
        "VocabularyDeclared",
        "VocabularyValuesAdded",
        "Extent",
        "PointVisibility",
        "ViewGroupDeclaration",
        "MetadataField",
        "ViewGroupDeclared",
        "PlainViewDeclaration",
        "PlainViewDeclared",
        "ViewRecord",
        "ViewCreated",
        "ViewDropped",
        "LayerDeclaration",
        "SuppliedContent",
        "LayerRegistered",
        "PublishRequest",
        "PublishedArtifact",
        "ContentItem",
        "Attachment",
        "ArtifactsPublished",
        "ShapeReport",
        "GrowRequest",
        "GrowingArtifact",
        "MembershipsGrown",
        "Status",
        "BatchLimits",
        "WriteExecutorStatus",
        "CompactionStatus",
    ] {
        let schema = &doc["components"]["schemas"][name];
        assert!(schema.is_object(), "schema {name} is missing");
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "{name} must be a closed object"
        );
    }
    // And the closure bites: an extra key on an otherwise valid envelope is rejected.
    assert_invalid(
        &doc,
        "Error",
        &json!({ "error": "unknown", "detail": "x", "extra": 1 }),
    );
    // The code list is closed too.
    assert_invalid(&doc, "Error", &json!({ "error": "teapot", "detail": "x" }));
}

/// Every 429 response in the document declares `Retry-After` as required, on every route that
/// can shed.
#[test]
fn every_429_in_the_description_requires_retry_after() {
    let doc = description();
    let backpressure = &doc["components"]["responses"]["Backpressure"];
    assert_eq!(
        backpressure["headers"]["Retry-After"]["required"],
        json!(true)
    );
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (_method, op) in item.as_object().unwrap() {
            if let Some(r) = op["responses"].get("429") {
                assert_eq!(
                    r["$ref"], "#/components/responses/Backpressure",
                    "{path}'s 429 must be the shared Backpressure response"
                );
            }
        }
    }
}

/// The `layers` field is an array or the literal string `"all"` in the schema, and nothing else.
/// What the server does with each is `viewport_membership.rs`' to assert.
#[test]
fn the_layers_field_is_an_array_or_the_string_all() {
    let doc = description();
    for value in [json!([]), json!(["clusters/a"]), json!("all")] {
        assert_valid(
            &doc,
            "ViewportRequest",
            &viewport_body(json!({ "layers": value })),
        );
    }
    for value in [json!("some"), json!(1), json!([1])] {
        assert_invalid(
            &doc,
            "ViewportRequest",
            &viewport_body(json!({ "layers": value })),
        );
    }
    // `artifact_budget` and `k = 0` are in the shape too.
    assert_valid(
        &doc,
        "ViewportRequest",
        &viewport_body(json!({ "k": 0, "artifact_budget": 3 })),
    );
    assert_invalid(&doc, "ViewportRequest", &viewport_body(json!({ "k": -1 })));
    assert_invalid(
        &doc,
        "ViewportRequest",
        &viewport_body(json!({ "zoom": 17 })),
    );
    assert_invalid(
        &doc,
        "ViewportRequest",
        &viewport_body(json!({ "unknown": 1 })),
    );
}

// ---------------------------------------------------------------------------------------------
// The session plane.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn authorise_and_revoke_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let token_id = auth["token_id"].as_u64().unwrap();

    // The token works on the viewer plane.
    let resp = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let authorise = |key: &str, body: Value| {
        f.server
            .client
            .post(f.server.session_url("/session/authorise"))
            .bearer_auth(key)
            .json(&body)
            .send()
    };
    let principal = principal_for(&f.server, &["0"]);

    // A key that is not accepted.
    let resp = authorise("tsk_nope_nope", json!({ "principal": principal }))
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
    // The operator credential mints for a principal, or for a set of terms, which a key may not.
    for body in [
        json!({ "principal": principal }),
        json!({ "terms": ["0", "public"] }),
    ] {
        assert_valid(&doc, "AuthoriseRequest", &body);
        let resp = authorise(OPERATOR_CREDENTIAL, body).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let minted: Value = resp.json().await.unwrap();
        assert_valid(&doc, "AuthoriseResponse", &minted);
        let resp = f
            .server
            .client
            .get(f.server.viewer_url("/v1/meta"))
            .bearer_auth(minted["token"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }
    let resp = authorise(&f.server.integrator_key, json!({ "terms": ["0"] }))
        .await
        .unwrap();
    assert_refusal(&doc, resp, 403, "forbidden").await;
    // A viewer's own session token is no credential here, so it cannot name its terms.
    let resp = authorise(token, json!({ "terms": ["0", "1"] })).await.unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
    // An accepted key whose principal lacks `authorise-as`.
    let catalogue = &f.server.state.catalogue;
    catalogue
        .create_principal("plain", tessera_catalogue::PrincipalKind::Service)
        .unwrap();
    let (plain, _) = catalogue.create_api_key("plain", None, None).unwrap();
    let resp = authorise(&plain.key, json!({ "principal": principal }))
        .await
        .unwrap();
    assert_refusal(&doc, resp, 403, "forbidden").await;
    // A principal that does not exist, one without `read`, and a body naming neither or both.
    let key = f.server.integrator_key.clone();
    let resp = authorise(&key, json!({ "principal": "nobody" })).await.unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = authorise(&key, json!({ "principal": "plain" })).await.unwrap();
    assert_refusal(&doc, resp, 403, "forbidden").await;
    for body in [
        json!({}),
        json!({ "principal": principal, "access_token": "x" }),
    ] {
        assert_invalid(&doc, "AuthoriseRequest", &body);
        let resp = authorise(&key, body).await.unwrap();
        assert_refusal(&doc, resp, 422, "contract").await;
    }
    let resp = authorise(&key, json!({ "access_token": "not.a.token" }))
        .await
        .unwrap();
    assert_refusal(&doc, resp, 422, "contract").await;

    // Revoke: 204 with no body, and the token is then a 403, the session having ended.
    let body = json!({ "token_id": token_id });
    assert_valid(&doc, "RevokeRequest", &body);
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/revoke"))
        .bearer_auth(&f.server.integrator_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert!(resp.bytes().await.unwrap().is_empty());
    let resp = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 403, "expired-token").await;

    // Revoke without a key, and with a key lacking `authorise-as`.
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/revoke"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/revoke"))
        .bearer_auth(&plain.key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 403, "forbidden").await;
}

/// `POST /v1/login` by password and by API key, its refusals, and `POST /v1/logout`.
#[tokio::test]
async fn login_and_logout_match_the_description() {
    use tessera_catalogue::{Grantee, Permission, PrincipalKind};
    let doc = description();
    let f = fixture().await;
    let catalogue = &f.server.state.catalogue;
    catalogue.create_principal("bea", PrincipalKind::Person).unwrap();
    catalogue
        .set_password("bea", "correct horse battery staple")
        .unwrap();
    catalogue
        .grant_permission(Grantee::Principal("bea"), Permission::Read)
        .unwrap();
    catalogue.grant_term(Grantee::Principal("bea"), "0").unwrap();
    let (key, _) = catalogue.create_api_key("bea", None, None).unwrap();
    catalogue.create_principal("cal", PrincipalKind::Person).unwrap();
    catalogue
        .set_password("cal", "correct horse battery staple")
        .unwrap();

    let login = |body: Value| {
        f.server
            .client
            .post(f.server.viewer_url("/v1/login"))
            .json(&body)
            .send()
    };
    let password = |principal: &str, password: &str| {
        json!({ "password": { "principal": principal, "password": password } })
    };

    for body in [
        password("bea", "correct horse battery staple"),
        json!({ "api_key": key.key }),
    ] {
        assert_valid(&doc, "LoginRequest", &body);
        let resp = login(body).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let answer: Value = resp.json().await.unwrap();
        assert_valid(&doc, "LoginResponse", &answer);
        let token = answer["token"].as_str().unwrap();
        let resp = f
            .server
            .client
            .get(f.server.viewer_url("/v1/meta"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        let resp = f
            .server
            .client
            .post(f.server.viewer_url("/v1/logout"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
        let resp = f
            .server
            .client
            .get(f.server.viewer_url("/v1/meta"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_refusal(&doc, resp, 403, "expired-token").await;
    }

    // A wrong password, an unknown name, a malformed key and an access token from no provider
    // each answer the same 401.
    for body in [
        password("bea", "the wrong password entirely"),
        password("nobody", "correct horse battery staple"),
        json!({ "api_key": "tsk_nope_nope" }),
        json!({ "access_token": "not.a.token" }),
    ] {
        let resp = login(body).await.unwrap();
        assert_refusal(&doc, resp, 401, "bad-credential").await;
    }
    // A principal without `read`.
    let resp = login(password("cal", "correct horse battery staple"))
        .await
        .unwrap();
    assert_refusal(&doc, resp, 403, "forbidden").await;
    // A credential of a kind the route does not take, and two credentials at once, which the
    // HTTP framework refuses before the handler runs.
    let body = json!({ "certificate": "x" });
    assert_invalid(&doc, "LoginRequest", &body);
    let resp = login(body).await.unwrap();
    assert_refusal(&doc, resp, 422, "contract").await;
    let body = json!({ "api_key": key.key, "access_token": "x" });
    assert_invalid(&doc, "LoginRequest", &body);
    let resp = login(body).await.unwrap();
    assert_framework_refusal(&doc, &reqwest::Method::POST, resp, 400).await;
}

/// Every catalogue verb on the control plane, with a success and a refusal each.
#[tokio::test]
async fn the_catalogue_routes_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let s = &f.server;
    let (get, post, put, patch, delete) = (
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::PUT,
        reqwest::Method::PATCH,
        reqwest::Method::DELETE,
    );
    let send = |method: &reqwest::Method, path: &str, body: Option<(&str, Value)>| {
        let mut req = control(s, method, path);
        if let Some((schema, body)) = body {
            if !schema.is_empty() {
                assert_valid(&doc, schema, &body);
            }
            req = req.json(&body);
        }
        req.send()
    };

    // Principals.
    let created = json!({ "name": "ann", "kind": "person" });
    let resp = send(&post, "/control/principals", Some(("PrincipalCreate", created.clone())));
    assert_answer(&doc, &post, resp.await.unwrap(), 200).await;
    let resp = send(&post, "/control/principals", Some(("PrincipalCreate", created)));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 409, "conflict").await;
    let body = json!({ "name": "bo", "kind": "robot" });
    assert_invalid(&doc, "PrincipalCreate", &body);
    let resp = send(&post, "/control/principals", Some(("", body)));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&get, "/control/principals", None);
    assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    let resp = send(&get, "/control/principals/ann", None);
    assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    let resp = send(&get, "/control/principals/nobody", None);
    assert_refusal_to(&doc, Some(&get), resp.await.unwrap(), 404, "unknown").await;
    let resp = send(
        &patch,
        "/control/principals/ann",
        Some(("PrincipalChange", json!({ "disabled": false }))),
    );
    assert_answer(&doc, &patch, resp.await.unwrap(), 200).await;
    assert_invalid(&doc, "PrincipalChange", &json!({}));
    let resp = send(&patch, "/control/principals/ann", Some(("", json!({}))));
    assert_refusal_to(&doc, Some(&patch), resp.await.unwrap(), 422, "contract").await;
    let resp = send(
        &patch,
        "/control/principals/nobody",
        Some(("PrincipalChange", json!({ "disabled": true }))),
    );
    assert_refusal_to(&doc, Some(&patch), resp.await.unwrap(), 404, "unknown").await;

    // Passwords.
    let body = json!({ "password": "correct horse battery staple" });
    let resp = send(&put, "/control/principals/ann/password", Some(("PasswordSet", body)));
    assert_answer(&doc, &put, resp.await.unwrap(), 200).await;
    let body = json!({ "password": "short" });
    let resp = send(&put, "/control/principals/ann/password", Some(("PasswordSet", body)));
    assert_refusal_to(&doc, Some(&put), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&delete, "/control/principals/ann/password", None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, "/control/principals/nobody/password", None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;

    // Keys.
    let body = json!({ "permissions": ["read"], "expires_at": 4_000_000_000u64 });
    let resp = send(&post, "/control/principals/ann/keys", Some(("KeyCreate", body)));
    let issued = assert_answer(&doc, &post, resp.await.unwrap(), 200).await;
    let body = json!({ "permissions": ["superuser"] });
    assert_invalid(&doc, "KeyCreate", &body);
    let resp = send(&post, "/control/principals/ann/keys", Some(("", body)));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&post, "/control/principals/nobody/keys", Some(("KeyCreate", json!({}))));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 404, "unknown").await;
    let resp = send(&get, "/control/principals/ann/keys", None);
    let keys = assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    assert_eq!(keys["keys"][0]["prefix"], issued["prefix"]);
    let resp = send(&get, "/control/principals/nobody/keys", None);
    assert_refusal_to(&doc, Some(&get), resp.await.unwrap(), 404, "unknown").await;
    let path = format!("/control/keys/{}", issued["prefix"].as_str().unwrap());
    let resp = send(&delete, &path, None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, &path, None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;

    // Groups and members.
    let body = json!({ "name": "analysts" });
    let resp = send(&post, "/control/groups", Some(("GroupCreate", body.clone())));
    assert_answer(&doc, &post, resp.await.unwrap(), 200).await;
    let resp = send(&post, "/control/groups", Some(("GroupCreate", body)));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 409, "conflict").await;
    let resp = send(&post, "/control/groups", Some(("GroupCreate", json!({ "name": " " }))));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&put, "/control/groups/analysts/members/ann", None);
    assert_answer(&doc, &put, resp.await.unwrap(), 200).await;
    let resp = send(&put, "/control/groups/analysts/members/nobody", None);
    assert_refusal_to(&doc, Some(&put), resp.await.unwrap(), 404, "unknown").await;
    let resp = send(&get, "/control/groups", None);
    assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    let resp = send(&get, "/control/groups/analysts", None);
    let group = assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    assert_eq!(group["members"], json!(["ann"]));
    let resp = send(&get, "/control/groups/nobody", None);
    assert_refusal_to(&doc, Some(&get), resp.await.unwrap(), 404, "unknown").await;
    let resp = send(&delete, "/control/groups/analysts/members/ann", None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, "/control/groups/nobody/members/ann", None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;

    // Grants.
    for (path, method) in [("/control/grants", &post), ("/control/grants/revoke", &post)] {
        for body in [
            json!({ "principal": "ann", "term": "0" }),
            json!({ "principal": "ann", "terms": ["0", "1"] }),
            json!({ "group": "analysts", "permission": "read" }),
        ] {
            let resp = send(method, path, Some(("Grant", body)));
            assert_answer(&doc, method, resp.await.unwrap(), 200).await;
        }
        let body = json!({ "principal": "ann" });
        assert_invalid(&doc, "Grant", &body);
        let resp = send(method, path, Some(("", body)));
        assert_refusal_to(&doc, Some(method), resp.await.unwrap(), 422, "contract").await;
        let resp = send(method, path, Some(("Grant", json!({ "group": "nobody", "term": "0" }))));
        assert_refusal_to(&doc, Some(method), resp.await.unwrap(), 404, "unknown").await;
    }
    let resp = send(&delete, "/control/groups/analysts", None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, "/control/groups/analysts", None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;

    // Providers.
    let declared = json!({
        "issuer": "https://login.example.org",
        "audience": "tessera",
        "jwks_url": "http://127.0.0.1:9/keys",
        "claim_rules": [{ "claim": "groups[*]", "template": "{value}" }],
        "role_mappings": [{ "claim": "groups[*]", "value": "tessera-admins", "group": "admins" }],
    });
    for _ in 0..2 {
        let resp = send(
            &put,
            "/control/providers/corp",
            Some(("ProviderDeclaration", declared.clone())),
        );
        assert_answer(&doc, &put, resp.await.unwrap(), 200).await;
    }
    let mut insecure = declared.clone();
    insecure["jwks_url"] = json!("http://login.example.org/keys");
    let resp = send(&put, "/control/providers/other", Some(("ProviderDeclaration", insecure)));
    assert_refusal_to(&doc, Some(&put), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&get, "/control/providers", None);
    assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    let resp = send(&get, "/control/providers/corp", None);
    let provider = assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    assert_eq!(provider["read_only"], false);
    let resp = send(&get, "/control/providers/nobody", None);
    assert_refusal_to(&doc, Some(&get), resp.await.unwrap(), 404, "unknown").await;
    let resp = send(&delete, "/control/providers/corp", None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, "/control/providers/corp", None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;

    // Sessions.
    let auth = authorise_checked(&doc, s, &["0"]).await;
    let viewer = principal_for(s, &["0"]);
    let resp = send(&get, &format!("/control/sessions?principal={viewer}"), None);
    let listed = assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    assert_eq!(listed["sessions"][0]["token_id"], auth["token_id"]);
    assert_eq!(listed["sessions"][0]["minted_by"], json!(INTEGRATOR));
    let resp = send(&get, "/control/sessions?principal=a&provider=b", None);
    assert_refusal_to(&doc, Some(&get), resp.await.unwrap(), 422, "contract").await;
    let resp = send(&get, "/control/sessions", None);
    assert_answer(&doc, &get, resp.await.unwrap(), 200).await;
    let resp = send(
        &post,
        "/control/sessions/end",
        Some(("SessionsEnd", json!({ "principal": viewer }))),
    );
    let ended = assert_answer(&doc, &post, resp.await.unwrap(), 200).await;
    assert_eq!(ended["sessions_ended"], 1);
    let resp = send(&post, "/control/sessions/end", Some(("", json!({}))));
    assert_refusal_to(&doc, Some(&post), resp.await.unwrap(), 422, "contract").await;

    let resp = send(&delete, "/control/principals/ann", None);
    assert_answer(&doc, &delete, resp.await.unwrap(), 200).await;
    let resp = send(&delete, "/control/principals/ann", None);
    assert_refusal_to(&doc, Some(&delete), resp.await.unwrap(), 404, "unknown").await;
}

/// Every control operation refuses an accepted credential without the permission it needs with
/// `403 forbidden`, before the body is read: a write needs `write`, flush and compaction `write`
/// and `write-all`, and status and the catalogue `admin`.
#[tokio::test]
async fn every_control_route_needs_its_permission() {
    use tessera_catalogue::{Grantee, Permission, PrincipalKind};
    let doc = description();
    let f = fixture().await;
    let catalogue = &f.server.state.catalogue;
    let key_for = |name: &str, permissions: &[Permission]| {
        catalogue.create_principal(name, PrincipalKind::Service).unwrap();
        for p in permissions {
            catalogue.grant_permission(Grantee::Principal(name), *p).unwrap();
        }
        catalogue.create_api_key(name, None, None).unwrap().0.key
    };
    let everything_but = |missing: &[Permission]| -> Vec<Permission> {
        Permission::ALL.into_iter().filter(|p| !missing.contains(p)).collect()
    };
    let nothing = key_for("nothing", &[]);
    let writer = key_for("writer", &[Permission::Write]);
    let flusher = key_for("flusher", &[Permission::Write, Permission::WriteAll]);
    let not_writer = key_for("not-writer", &everything_but(&[Permission::Write]));
    let not_admin = key_for("not-admin", &everything_but(&[Permission::Admin]));
    let not_write_all = key_for("not-write-all", &everything_but(&[Permission::WriteAll]));
    let admin = key_for("admin", &[Permission::Admin]);

    let (mut writes, mut operations) = (0, 0);
    for (path, item) in doc["paths"].as_object().unwrap() {
        if !path.starts_with("/control/") {
            continue;
        }
        for (method, op) in item.as_object().unwrap() {
            let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let concrete = path
                .split('/')
                .map(|s| if s.starts_with('{') { "1" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            let tag = op["tags"][0].as_str().unwrap_or("");
            // Each credential, and whether the route refuses it.
            let cases: Vec<(&str, &str, bool)> = match tag {
                "control: flush" | "control: compact" => {
                    operations += 1;
                    vec![
                        ("write", &writer, true),
                        ("all but write", &not_writer, true),
                        ("all but write-all", &not_write_all, true),
                        ("admin", &admin, true),
                        ("write and write-all", &flusher, false),
                    ]
                }
                "control: identity" | "control: status" => vec![
                    ("write", &writer, true),
                    ("all but admin", &not_admin, true),
                    ("admin", &admin, false),
                ],
                _ => {
                    writes += 1;
                    vec![
                        ("all but write", &not_writer, true),
                        ("admin", &admin, true),
                        ("write", &writer, false),
                    ]
                }
            };
            for (who, key, refused) in [("nothing", nothing.as_str(), true)].into_iter().chain(cases) {
                let resp = f
                    .server
                    .client
                    .request(method.clone(), f.server.control_url(&concrete))
                    .bearer_auth(key)
                    .send()
                    .await
                    .unwrap();
                if refused {
                    assert_refusal_to(&doc, Some(&method), resp, 403, "forbidden").await;
                } else {
                    assert_ne!(resp.status().as_u16(), 403, "{method} {path} with {who}");
                }
            }
        }
    }
    assert_eq!(writes, 13, "ingest, changes, the declarations, layers and artifacts");
    assert_eq!(operations, 2, "flush and compact");
}

#[tokio::test]
async fn a_saturated_gate_sheds_authorise_with_the_described_429() {
    let doc = description();
    let tmp = TempDir::new().unwrap();
    let bundle_root = copy_bundle(&tmp);
    // No slots at all: every admission is shed before any wait.
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        default_engine_config(),
        ComputeGate::new(0, 0, 250),
    )
    .await;
    let principal = principal_for(&server, &["0"]);
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&json!({ "principal": principal }))
        .send()
        .await
        .unwrap();
    let body = assert_refusal(&doc, resp, 429, "backpressure").await;
    assert_eq!(
        body["retry_after_s"],
        json!(1),
        "the compute gate's figure is 1, fixed"
    );
}

#[tokio::test]
async fn the_probes_are_bare_status_codes_on_both_listeners() {
    let f = fixture().await;
    for url in [
        f.server.viewer_url("/healthz"),
        f.server.viewer_url("/readyz"),
        f.server.session_url("/healthz"),
        f.server.session_url("/readyz"),
    ] {
        let resp = f.server.client.get(&url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{url}");
        assert!(
            resp.bytes().await.unwrap().is_empty(),
            "{url} carries no body"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The viewer plane.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn meta_matches_the_description_and_requires_a_token() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let meta: Value = resp.json().await.unwrap();
    assert_valid(&doc, "Meta", &meta);
    // The fixture's shape is in the document's terms: a category column, a numeric operand, and
    // the layer registered above — so the sub-schemas were exercised rather than vacuously passed.
    assert!(meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["category"].is_object()));
    assert!(meta["filter_operands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["family"] == "numeric"));
    assert_eq!(meta["layers"].as_array().unwrap().len(), 1);

    let resp = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
}

#[tokio::test]
async fn categories_match_the_description_in_both_forms_and_both_refusals() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let get = |path: String| {
        f.server
            .client
            .get(f.server.viewer_url(&path))
            .bearer_auth(token)
            .send()
    };

    // The bare form, paged.
    let resp = get("/v1/categories/archive?limit=2".to_string())
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page: Value = resp.json().await.unwrap();
    assert_valid(&doc, "CategoriesResponse", &page);
    assert_eq!(page["values"].as_array().unwrap().len(), 2);
    assert!(page["next"].is_string());

    // The codes form: `next` is always null.
    let resp = get("/v1/categories/archive?codes=11,33".to_string())
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resolved: Value = resp.json().await.unwrap();
    assert_valid(&doc, "CategoriesResponse", &resolved);
    assert_eq!(resolved["values"].as_array().unwrap().len(), 2);
    assert!(resolved["next"].is_null());

    // Refusals: a column that is not a category is 404; limit=0 is 422.
    let resp = get("/v1/categories/score".to_string()).await.unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = get("/v1/categories/archive?limit=0".to_string())
        .await
        .unwrap();
    assert_refusal(&doc, resp, 422, "contract").await;
}

/// The typeahead's own shape: `SuggestResponse` on a real page, `count` present iff asked, and
/// its refusals, among them the one `/v1/categories/{column}` does not have: `q` over 256 bytes is
/// `422`.
#[tokio::test]
async fn suggest_matches_the_description_and_its_own_refusals() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let get = |path: String| {
        f.server
            .client
            .get(f.server.viewer_url(&path))
            .bearer_auth(token)
            .send()
    };

    // No `count` unless asked.
    let resp = get("/v1/categories/archive/suggest?q=a".to_string())
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page: Value = resp.json().await.unwrap();
    assert_valid(&doc, "SuggestResponse", &page);
    assert_eq!(page["column"], "archive");
    assert_eq!(page["q"], "a");
    for value in page["values"].as_array().unwrap() {
        assert!(
            value.get("count").is_none(),
            "count must be absent without `counts=true`: {value}"
        );
    }

    // `counts=true` puts a `count` on every served value.
    let resp = get("/v1/categories/archive/suggest?q=a&counts=true".to_string())
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page: Value = resp.json().await.unwrap();
    assert_valid(&doc, "SuggestResponse", &page);
    assert!(!page["values"].as_array().unwrap().is_empty());
    for value in page["values"].as_array().unwrap() {
        assert!(
            value["count"].is_u64(),
            "counts=true must put an exact count on every served value: {value}"
        );
    }

    // An empty `q` matches every value, in index order, with no cursor to page through them.
    let resp = get("/v1/categories/archive/suggest?q=".to_string())
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page: Value = resp.json().await.unwrap();
    assert_valid(&doc, "SuggestResponse", &page);
    assert!(!page["values"].as_array().unwrap().is_empty());

    // Refusals: a column that is not a category is 404; `limit=0`, `q` over 256 bytes, and an
    // unknown query parameter are all 422.
    let resp = get("/v1/categories/score/suggest?q=a".to_string())
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&reqwest::Method::GET), resp, 404, "unknown").await;
    let resp = get("/v1/categories/archive/suggest?q=a&limit=0".to_string())
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&reqwest::Method::GET), resp, 422, "contract").await;
    let long_q = "x".repeat(257);
    let resp = get(format!("/v1/categories/archive/suggest?q={long_q}"))
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&reqwest::Method::GET), resp, 422, "contract").await;
    let resp = get("/v1/categories/archive/suggest?q=a&bogus=1".to_string())
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&reqwest::Method::GET), resp, 422, "contract").await;

    // The `POST` form: a body the description accepts, a page it describes with the region's
    // verdict, and its own refusal, `filters` without `view`.
    let post = |body: Value| {
        assert_valid(&doc, "SuggestRequest", &body);
        f.server
            .client
            .post(f.server.viewer_url("/v1/categories/archive/suggest"))
            .bearer_auth(token)
            .json(&body)
            .send()
    };
    let filters = json!({ "all_of": [
        { "archive": { "in": ["astro", "cond"] } },
        { "region": { "bbox": [100.0, 100.0, 900.0, 900.0] } },
    ] });
    let resp = post(json!({ "q": "", "counts": true, "view": "s0", "filters": filters }))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["x-tessera-region"], "exact");
    let page: Value = resp.json().await.unwrap();
    assert_valid(&doc, "SuggestResponse", &page);
    let hep = page["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "hep")
        .expect("hep is offered");
    assert_eq!(hep["count"], 0, "the filter excludes hep: {page}");
    let resp = post(json!({ "q": "", "counts": true, "filters": filters }))
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&reqwest::Method::POST), resp, 422, "contract").await;
}

#[tokio::test]
async fn viewport_carries_the_described_headers_and_framing() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    // The full request shape, validated, with the bbox form and every optional field but the
    // ruled string form of `layers`.
    let body = viewport_body(json!({
        "filters": { "all_of": [ { "archive": { "in": ["astro", 22] } },
                                 { "score": { "range": { "gte": 0 } } } ] },
        "underlay_offset": 1,
        "layers": [LAYER],
        "artifact_budget": 10,
    }));
    assert_valid(&doc, "ViewportRequest", &body);
    let resp = viewport(&f.server, token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/octet-stream"
    );
    let headers = doc["paths"]["/v1/viewport"]["post"]["responses"]["200"]["headers"]
        .as_object()
        .unwrap();
    for (name, spec) in headers {
        // A header the description marks optional — `x-tessera-region`, present exactly when
        // the request carried a region leaf — is checked on the request that asks for it below.
        if spec["required"].as_bool() == Some(true) {
            assert!(
                resp.headers().contains_key(name.as_str()),
                "response lacks the described header {name}"
            );
        } else {
            assert!(
                !resp.headers().contains_key(name.as_str()),
                "the optional header {name} appeared on a request that did not ask for it"
            );
        }
    }
    let stale = resp.headers()["x-tessera-stale"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(stale == "0" || stale == "1");
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "etag is quoted: {etag}"
    );
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(!decoded.tiles.is_empty());
    assert!(
        decoded.sub_cells.is_some(),
        "underlay requested, so the kind-2 frame is present"
    );

    // The region leaf, and the one header it brings (`selection-operand.md` §6): present exactly
    // when asked for, and one of the two spellings the description gives it.
    let region_body = viewport_body(json!({
        "filters": { "all_of": [ { "region": { "bbox": [100.0, 100.0, 900.0, 900.0] } },
                                 { "none_of": [ { "region": { "circle": [500.0, 500.0, 50.0], "space": "view" } } ] } ] },
    }));
    assert_valid(&doc, "ViewportRequest", &region_body);
    let region_resp = viewport(&f.server, token, &region_body).await;
    assert_eq!(region_resp.status().as_u16(), 200);
    let verdict = region_resp
        .headers()
        .get("x-tessera-region")
        .expect("a request carrying a region leaf answers with the verdict")
        .to_str()
        .unwrap()
        .to_string();
    // The description's pattern, `^(exact|cover; depth=[0-9]+)$`, checked by hand rather than
    // through a regex crate this test suite does not otherwise carry.
    let described = verdict == "exact"
        || verdict
            .strip_prefix("cover; depth=")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    assert!(
        described,
        "x-tessera-region {verdict:?} is not one of the described spellings"
    );
    assert_eq!(verdict, "exact");
    let artifacts = decoded
        .artifacts
        .expect("the named layer is reachable, so kind 5 is present");
    assert_eq!(artifacts.len(), 2);
    assert!(decoded.trailer.is_object());

    // `k = 0`: the counts-only request. Tiles and artifacts as before, no points frame at all.
    let body = viewport_body(json!({ "k": 0, "layers": [LAYER] }));
    assert_valid(&doc, "ViewportRequest", &body);
    let resp = viewport(&f.server, token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(!decoded.tiles.is_empty());
    assert_eq!(decoded.point_frames, 0, "k = 0 emits no points frame");
    assert_eq!(decoded.points.len(), 0);
    assert_eq!(decoded.trailer["points"], json!(0));
    assert_eq!(decoded.artifacts.map(|a| a.len()), Some(2));

    // `layers: []`: no artifact pass, so no kind-5 frame — absent, never empty.
    let resp = viewport(&f.server, token, &viewport_body(json!({ "layers": [] }))).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(decoded.artifacts.is_none());
    assert!(
        decoded.sub_cells.is_none(),
        "no underlay requested, so no kind-2 frame"
    );

    // A name this principal does not reach is intersected away — the narrow principal is served
    // only the artifact it clears, and a made-up name beside the real one changes nothing.
    let narrow = authorise_checked(&doc, &f.server, &["1"]).await;
    let narrow_token = narrow["token"].as_str().unwrap();
    let resp = viewport(
        &f.server,
        narrow_token,
        &viewport_body(json!({ "k": 0, "layers": [LAYER, "no/such/layer"] })),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    let served = decoded.artifacts.unwrap();
    assert_eq!(served.len(), 1, "the narrow principal clears c0 only");
    assert_eq!(served[0].key.as_deref(), Some("c0"));

    // The tiles form.
    let body = json!({ "view": "s0", "zoom": 1, "tiles": [0, 3], "k": 5 });
    assert_valid(&doc, "ViewportRequest", &body);
    let resp = viewport(&f.server, token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert_eq!(decoded.tiles.len(), 2);

    // Refusals.
    let resp = viewport(&f.server, token, &viewport_body(json!({ "tiles": [0] }))).await;
    assert_refusal(&doc, resp, 422, "contract").await;
    let resp = viewport(&f.server, token, &viewport_body(json!({ "zoom": 17 }))).await;
    assert_refusal(&doc, resp, 422, "contract").await;
    let resp = viewport(
        &f.server,
        token,
        &viewport_body(json!({ "filters": { "no_such_column": { "eq": 1 } } })),
    )
    .await;
    assert_refusal(&doc, resp, 422, "contract").await;
    let resp = viewport(
        &f.server,
        token,
        &viewport_body(json!({ "view": "no-such-view" })),
    )
    .await;
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = viewport(&f.server, "not-a-token", &viewport_body(json!({}))).await;
    assert_refusal(&doc, resp, 401, "bad-credential").await;
}

#[tokio::test]
async fn items_match_the_description_with_every_refusal() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    // An identifier this principal can see, taken from a viewport response.
    let resp = viewport(&f.server, token, &viewport_body(json!({}))).await;
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    let (tessera_id, _) = decoded.points[0];

    let body = json!({});
    assert_valid(&doc, "ItemRequest", &body);
    let post = |id: u64, body: Value, tok: &str| {
        f.server
            .client
            .post(f.server.viewer_url(&format!("/v1/items/{id}")))
            .bearer_auth(tok)
            .json(&body)
            .send()
    };
    let resp = post(tessera_id, body, token).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let item: Value = resp.json().await.unwrap();
    assert_valid(&doc, "ItemResponse", &item);
    assert!(
        item["fields"]["archive"]
            .as_str()
            .is_some_and(|k| !k.is_empty()),
        "a category arrives as its key: {item}"
    );

    // Refusals: an item nobody with zero terms may see is a 404 (identical to nonexistence).
    let nobody = authorise_checked(&doc, &f.server, &[]).await;
    let resp = post(tessera_id, json!({}), nobody["token"].as_str().unwrap())
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    // The request schema refuses what the server refuses: an unknown field.
    assert_invalid(&doc, "ItemRequest", &json!({ "unknown": 1 }));
}

/// `POST /v1/artifacts`: the request shape, the described headers, the three JSON frames against
/// their schemas, and the refusals.
#[tokio::test]
async fn the_artifacts_read_matches_the_description_with_its_refusals() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let post = |body: Value, tok: &str| {
        f.server
            .client
            .post(f.server.viewer_url("/v1/artifacts"))
            .bearer_auth(tok)
            .json(&body)
            .send()
    };

    let body = json!({
        "view": "s0",
        "layer": LAYER,
        "fields": ["key", "level", "parents", "target", "masked_count", "content", "centroid",
                   "box", "shape"],
        "filters": { "score": { "range": { "gte": 1 } } },
        "keep_unmatched": true,
        "count": true,
        "page_rows": 1,
        "pages": 1,
        "compression": "zstd",
    });
    assert_valid(&doc, "ArtifactsRequest", &body);
    let resp = post(body, token).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let headers = doc["paths"]["/v1/artifacts"]["post"]["responses"]["200"]["headers"]
        .as_object()
        .unwrap();
    for (name, spec) in headers {
        if spec["required"].as_bool() == Some(true) {
            assert!(resp.headers().contains_key(name.as_str()), "{name}");
        }
    }
    let decoded = decode_records(&resp.bytes().await.unwrap());
    assert_valid(&doc, "ArtifactsHead", &decoded.head);
    assert!(decoded.head["served"].is_u64() && decoded.head["matched"].is_u64());
    assert_eq!(decoded.pages.len(), 1);
    for (_, end) in &decoded.pages {
        assert_valid(&doc, "PageEnd", end);
    }
    assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
    let cursor = decoded.trailer["next"].as_str().unwrap().to_string();

    let resp = post(
        json!({ "view": "s0", "layer": LAYER, "fields": ["key"], "parent": f.artifacts[0],
                "cursor": cursor }),
        token,
    )
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_records(&resp.bytes().await.unwrap());
    assert_valid(&doc, "ArtifactsHead", &decoded.head);
    assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
    assert!(decoded.trailer["next"].is_null());

    for body in [
        json!({ "view": "s0", "layer": LAYER, "fields": [], "page_rows": 0 }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "unknown": 1 }),
        json!({ "view": "s0", "layer": LAYER, "fields": ["size"] }),
        json!({ "view": "s0", "layer": "clusters/none", "fields": [] }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "level": 0 }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "parent": "1", "q": "c" }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "cursor": "not-a-cursor" }),
    ] {
        let resp = post(body, token).await.unwrap();
        assert_refusal(&doc, resp, 422, "contract").await;
    }
    let resp = post(json!({ "view": "no-such-view", "layer": LAYER, "fields": [] }), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;

    for body in [
        json!({ "view": "s0", "layer": LAYER, "fields": [], "unknown": 1 }),
        json!({ "view": "s0", "fields": [] }),
        json!({ "view": "s0", "layer": LAYER, "fields": ["size"] }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "pages": 0 }),
        json!({ "view": "s0", "layer": LAYER, "fields": [], "compression": "gzip" }),
    ] {
        assert_invalid(&doc, "ArtifactsRequest", &body);
    }
}

/// `POST /v1/items`: the request shape, the described headers, the three JSON frames against
/// their schemas, and the refusals.
#[tokio::test]
async fn the_items_read_matches_the_description_with_its_refusals() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let post = |body: Value, tok: &str| {
        f.server
            .client
            .post(f.server.viewer_url("/v1/items"))
            .bearer_auth(tok)
            .json(&body)
            .send()
    };

    let body = json!({
        "view": "s0",
        "fields": ["archive", "score"],
        "system_fields": ["position", "labels"],
        "filters": { "score": { "range": { "gte": 5 } } },
        "keep_unmatched": true,
        "count": true,
        "order": "map",
        "page_rows": 10,
        "pages": 3,
        "compression": "zstd",
    });
    assert_valid(&doc, "ItemsRequest", &body);
    let resp = post(body, token).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/octet-stream"
    );
    let headers = doc["paths"]["/v1/items"]["post"]["responses"]["200"]["headers"]
        .as_object()
        .unwrap();
    for (name, spec) in headers {
        if spec["required"].as_bool() == Some(true) {
            assert!(resp.headers().contains_key(name.as_str()), "{name}");
        }
    }
    // The optional headers: the identity coordinate is present whenever a page was walked, and
    // the region verdict only with a region leaf, which this request has not.
    assert!(resp.headers().contains_key("x-tessera-identity-key"));
    assert!(!resp.headers().contains_key("x-tessera-region"));
    let decoded = decode_records(&resp.bytes().await.unwrap());
    assert_valid(&doc, "ItemsHead", &decoded.head);
    assert!(decoded.head["visible"].is_u64() && decoded.head["matched"].is_u64());
    assert_eq!(decoded.pages.len(), 3);
    for (_, end) in &decoded.pages {
        assert_valid(&doc, "PageEnd", end);
    }
    assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
    let cursor = decoded.trailer["next"].as_str().unwrap().to_string();

    // The rest of the read, from the cursor, to its end.
    let resp = post(json!({ "view": "s0", "fields": [], "cursor": cursor }), token)
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_records(&resp.bytes().await.unwrap());
    assert_valid(&doc, "ItemsHead", &decoded.head);
    assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
    assert!(decoded.trailer["next"].is_null());
    assert_eq!(decoded.trailer["ended_by"], "end");

    // Refusals.
    for body in [
        json!({ "view": "s0", "fields": [], "page_rows": 0 }),
        json!({ "view": "s0", "fields": [], "unknown": 1 }),
        json!({ "view": "s0", "fields": ["no_such_field"] }),
        json!({ "view": "s0", "fields": [], "cursor": "not-a-cursor" }),
    ] {
        let resp = post(body, token).await.unwrap();
        assert_refusal(&doc, resp, 422, "contract").await;
    }
    let resp = post(json!({ "view": "no-such-view", "fields": [] }), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = post(json!({ "view": "s0", "fields": [] }), "not-a-token")
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;

    // The request schema refuses what the server refuses.
    for body in [
        json!({ "view": "s0", "fields": [], "unknown": 1 }),
        json!({ "view": "s0" }),
        json!({ "view": "s0", "fields": [], "page_rows": 0 }),
        json!({ "view": "s0", "fields": [], "order": "random" }),
        json!({ "view": "s0", "fields": [], "compression": "gzip" }),
        json!({ "view": "s0", "fields": [], "system_fields": ["entity_id"] }),
    ] {
        assert_invalid(&doc, "ItemsRequest", &body);
    }
}

/// `POST /v1/aggregate`: the request shape, the described headers, the table heads, page ends and
/// trailer against their schemas, a read resumed from its cursor, and the refusals.
#[tokio::test]
async fn the_aggregate_read_matches_the_description_with_its_refusals() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let post = |body: Value, tok: &str| {
        f.server
            .client
            .post(f.server.viewer_url("/v1/aggregate"))
            .bearer_auth(tok)
            .json(&body)
            .send()
    };

    let body = json!({
        "view": "s0",
        "filters": { "region": { "bbox": [0.0, 0.0, 600.0, 1000.0] } },
        "reference": {},
        "groupings": [
            {},
            { "by": { "field": "archive", "top": 2 } },
            { "by": { "field": "archive", "values": ["hep", "astro"] }, "cells": { "depth": 8 } },
            { "by": { "layer": LAYER, "top": 5 } },
            { "by": { "layer": LAYER, "artifacts": [f.artifacts[1].clone(), 7] } },
            { "cells": { "depth": 10 } },
            { "cells": { "depth": 20, "area": [100.0, 100.0, 100.4, 100.4] } },
        ],
        "page_rows": 50,
        "pages": 4,
        "compression": "zstd",
    });
    assert_valid(&doc, "AggregateRequest", &body);
    let resp = post(body.clone(), token).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/octet-stream"
    );
    let headers = doc["paths"]["/v1/aggregate"]["post"]["responses"]["200"]["headers"]
        .as_object()
        .unwrap();
    for (name, spec) in headers {
        if spec["required"].as_bool() == Some(true) {
            assert!(resp.headers().contains_key(name.as_str()), "{name}");
        }
    }
    assert!(resp.headers().contains_key("x-tessera-identity-key"));
    assert_eq!(resp.headers()["x-tessera-region"], "exact");
    let decoded = decode_aggregate(&resp.bytes().await.unwrap());
    assert!(!decoded.tables.is_empty());
    for (head, pages) in &decoded.tables {
        assert_valid(&doc, "AggregateTableHead", head);
        for (_, end) in pages {
            assert_valid(&doc, "PageEnd", end);
        }
    }
    assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
    let mut cursor = decoded.trailer["next"].as_str().unwrap().to_string();

    // The rest of the read, from the cursor, to its end.
    let mut resumed = 0;
    loop {
        let mut next = body.clone();
        next["cursor"] = json!(cursor);
        next["pages"] = json!(1);
        assert_valid(&doc, "AggregateRequest", &next);
        let resp = post(next, token).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let decoded = decode_aggregate(&resp.bytes().await.unwrap());
        for (head, pages) in &decoded.tables {
            assert_valid(&doc, "AggregateTableHead", head);
            resumed += usize::from(head["resumed"] == true);
            for (_, end) in pages {
                assert_valid(&doc, "PageEnd", end);
            }
        }
        assert_valid(&doc, "RecordsTrailer", &decoded.trailer);
        match decoded.trailer["next"].as_str() {
            Some(next) => cursor = next.to_string(),
            None => break,
        }
    }
    assert!(resumed > 0, "a response continued a table");

    // Refusals.
    for body in [
        json!({ "view": "s0", "groupings": [] }),
        json!({ "view": "s0", "groupings": [{}], "unknown": 1 }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "archive" } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "score", "top": 1 } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "layer": LAYER, "top": 1, "level": 0 } }] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 33 } }] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 300 } }] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 11 } }] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 3, "area": [9.0, 0.0, 1.0, 1.0] } }] }),
        json!({ "view": "s0", "groupings": [{}], "cursor": "not-a-cursor" }),
    ] {
        let resp = post(body, token).await.unwrap();
        assert_refusal(&doc, resp, 422, "contract").await;
    }
    let resp = post(json!({ "view": "no-such-view", "groupings": [{}] }), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = post(json!({ "view": "s0", "groupings": [{}] }), "not-a-token")
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;

    // The request schema refuses what the server refuses.
    for body in [
        json!({ "view": "s0", "groupings": [{}], "unknown": 1 }),
        json!({ "view": "s0" }),
        json!({ "groupings": [{}] }),
        json!({ "view": "s0", "groupings": [] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 33 } }] }),
        json!({ "view": "s0", "groupings": [{ "cells": { "depth": 3, "area": [0.0, 0.0, 1.0] } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "archive" } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "archive", "top": 0 } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "archive", "values": [] } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "a", "layer": "b", "top": 1 } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "a", "top": 1, "values": ["x"] } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "field": "a", "artifacts": [1] } }] }),
        json!({ "view": "s0", "groupings": [{ "by": { "layer": "a", "values": ["x"] } }] }),
        json!({ "view": "s0", "groupings": [{}], "reference": { "a": 1, "b": 2 } }),
        json!({ "view": "s0", "groupings": [{}], "compression": "gzip" }),
    ] {
        assert_invalid(&doc, "AggregateRequest", &body);
    }
}

#[tokio::test]
async fn artifacts_match_the_description_with_one_refusal_shape() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let post = |id: &str, body: Value, tok: &str| {
        f.server
            .client
            .post(f.server.viewer_url(&format!("/v1/artifacts/{id}")))
            .bearer_auth(tok)
            .json(&body)
            .send()
    };

    let body = json!({ "view": "s0" });
    assert_valid(&doc, "ArtifactRequest", &body);
    let resp = post(&f.artifacts[0], body.clone(), token).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let artifact: Value = resp.json().await.unwrap();
    assert_valid(&doc, "ArtifactResponse", &artifact);
    assert_eq!(artifact["layer"], LAYER);
    assert_eq!(artifact["key"], "c0");
    assert_eq!(artifact["masked_count"], json!(12));
    assert!(
        artifact["centroid"].is_array()
            && artifact["box"].is_array()
            && artifact["shape"].is_array()
    );
    assert!(
        artifact.get("content").is_none(),
        "no supplied content declared, so absent"
    );

    // The masked count is the asking principal's: the narrow one sees c0 whole (its members all
    // carry term 0 and 1 alike) but c1 not at all.
    let narrow = authorise_checked(&doc, &f.server, &["1"]).await;
    let narrow_token = narrow["token"].as_str().unwrap();
    let resp = post(&f.artifacts[1], body.clone(), narrow_token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    // Same shape for a point's identifier and an unknown view.
    let resp = viewport(&f.server, token, &viewport_body(json!({}))).await;
    let (point_id, _) = decode_viewport_frames(&resp.bytes().await.unwrap()).points[0];
    let resp = post(&point_id.to_string(), body.clone(), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = post(&f.artifacts[0], json!({ "view": "no-such-view" }), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    // `view` is required by the schema, as by the server.
    assert_invalid(&doc, "ArtifactRequest", &json!({}));
}

/// **Every described route refuses a caller without its plane's credential. A viewer-plane route
/// refuses one with no session token, and one whose credential is not a token, whatever else is
/// wrong with the request; a control-plane route refuses one without the operator credential. The
/// routes are enumerated from the description, not listed by hand.**
///
/// The viewer half is the counterpart of
/// `every_path_on_the_control_listener_needs_the_credential` (`tests/http_write.rs`): a per-route
/// 401 test stays green while a new route ships open. The control plane checks its credential in
/// a layer over the whole router; the viewer plane checks it in the `ViewerSession` extractor,
/// which each handler names as its first argument after the state. A handler written without it
/// is caught by nothing but this test.
///
/// **What is enumerated.** axum exposes no route enumeration, so the loop runs over
/// `docs/openapi/tessera.yaml`'s operations and their declared `security`.
/// [`the_description_names_every_route_on_the_three_planes_and_no_other`] fails if a route is
/// mounted and not described; describing it means declaring its `security`; declaring
/// `sessionToken` puts it in this loop. A route declared `security: []` is skipped, and the two
/// probes are asserted below to be the only ones. A route declared `operatorCredential` is sent
/// with no credential and a wrong one and must answer the envelope's `401`.
///
/// **Each route is probed twice over.** Once with a request the route accepts, and once with each
/// way it can be malformed: a query string that does not parse or names a parameter the route does
/// not define, a body that is not JSON, a body with the wrong content type, JSON of the wrong
/// shape, an identifier that is not a number.
/// Without a valid token every one of them answers 401: the token is checked before the query
/// string, the path or the body is read, so an unauthenticated caller learns nothing about what
/// the route would have accepted and no body is buffered for them. With a valid token the same
/// malformed requests answer the refusal the route gives them, so the 401s are not the route
/// refusing everything.
///
/// **Mutations this kills:** dropping the `ViewerSession` argument from any viewer handler;
/// moving it after an `ApiQuery`, `Path` or `ApiJson` argument; mounting a new viewer route with
/// no credential check; answering a bare `StatusCode::UNAUTHORIZED` instead of
/// `ApiError::BadCredential`'s envelope; and, on the probe half, putting `/healthz` or `/readyz`
/// behind the credential.
#[tokio::test]
async fn every_described_route_requires_its_planes_credential() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let mut gated = 0usize;
    let mut probes = 0usize;
    let mut login = 0usize;
    let mut session_plane = 0usize;
    let mut control_plane = 0usize;

    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let security = op["security"]
                .as_array()
                .unwrap_or_else(|| panic!("{method} {path} declares no `security`"));
            let scheme = security
                .first()
                .map(|s| s.as_object().unwrap().keys().next().unwrap().clone());
            let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();

            match scheme.as_deref() {
                // An `authorise-as` key, on the session listener.
                Some("authoriseAsKey") => {
                    session_plane += 1;
                    continue;
                }
                // A control credential, on the control listener, answered 401 before anything
                // else about the request is read.
                Some("controlCredential") => {
                    control_plane += 1;
                    let concrete = path
                        .split('/')
                        .map(|s| if s.starts_with('{') { "1" } else { s })
                        .collect::<Vec<_>>()
                        .join("/");
                    for credential in [None, Some("not-the-operator-credential")] {
                        let mut req = f
                            .server
                            .client
                            .request(method.clone(), f.server.control_url(&concrete));
                        if let Some(credential) = credential {
                            req = req.bearer_auth(credential);
                        }
                        let resp = req.send().await.unwrap();
                        assert_refusal_to(&doc, Some(&method), resp, 401, "bad-credential").await;
                    }
                    continue;
                }
                // Login takes its credential in the body, and is tested on its own.
                None if path == "/v1/login" => {
                    login += 1;
                    continue;
                }
                // The probes answer without a credential, and they are the only other routes on
                // this plane that do.
                None => {
                    probes += 1;
                    let resp =
                        send_viewer_probe(&f.server, &method, path, Malformed::No, None).await;
                    assert_ne!(
                        resp.status().as_u16(),
                        401,
                        "{method} {path} is described as unauthenticated and must not demand a \
                         credential"
                    );
                    continue;
                }
                // Logout ends the session it is sent with, so only its refusals are probed here.
                Some("sessionToken") if path == "/v1/logout" => {
                    gated += 1;
                    for credential in [None, Some("not-a-session-token")] {
                        let resp =
                            send_viewer_probe(&f.server, &method, path, Malformed::No, credential)
                                .await;
                        assert_refusal_to(&doc, Some(&method), resp, 401, "bad-credential").await;
                    }
                    continue;
                }
                Some("sessionToken") => gated += 1,
                Some(other) => panic!("{method} {path} declares an unknown scheme {other}"),
            }

            let mut kinds = vec![(Malformed::No, 0)];
            kinds.extend(malformed_viewer_requests(&method, path));
            for (kind, with_token) in kinds {
                for credential in [None, Some("not-a-session-token")] {
                    let resp = send_viewer_probe(&f.server, &method, path, kind, credential).await;
                    assert_eq!(
                        resp.status().as_u16(),
                        401,
                        "{method} {path} ({kind:?}) answered {} for credential {credential:?}; \
                         every viewer route requires a session token before it reads the request",
                        resp.status()
                    );
                    assert_refusal_to(&doc, Some(&method), resp, 401, "bad-credential").await;
                }
                if kind != Malformed::No {
                    let resp =
                        send_viewer_probe(&f.server, &method, path, kind, Some(token)).await;
                    if with_token == 422 {
                        assert_refusal_to(&doc, Some(&method), resp, with_token, "contract").await;
                    } else {
                        assert_eq!(
                            resp.status().as_u16(),
                            with_token,
                            "{method} {path} ({kind:?}) with a valid token"
                        );
                    }
                }
            }
        }
    }

    // Non-vacuity, both halves: the loop must have found the seven gated routes and the two
    // probes, or it enumerated nothing and proved nothing.
    assert_eq!(
        gated, 12,
        "the viewer plane's gated operations are logout, meta, categories, suggest's two forms, \
         viewport, the items read, items, the artifacts read, artifacts, artifacts/browse and the \
         aggregate; a change to that set belongs in this test's reasoning, not silently in its \
         count"
    );
    assert_eq!(
        probes, 2,
        "/healthz and /readyz are the only unauthenticated routes besides login"
    );
    assert_eq!(login, 1);
    assert_eq!(
        session_plane, 2,
        "/session/authorise and /session/revoke are the session plane's"
    );
    assert_eq!(
        control_plane, 40,
        "the control plane's operations are ingest, changes, status, flush, compact, \
         the attribute, vocabulary, value-page, view group and plain view declarations, a group \
         view's create and drop, a layer's registration and drop, an artifact publication \
         and growth, and the catalogue's 24 verbs"
    );
}

/// One way a viewer request can be wrong before its handler runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Malformed {
    /// A request the route accepts.
    No,
    /// A query string whose `limit` is not a number.
    Query,
    /// A query string naming a parameter the route does not define.
    UnknownQuery,
    /// A body that is not JSON.
    Syntax,
    /// A JSON body sent as `text/plain`.
    ContentType,
    /// JSON that is not the request object.
    Shape,
    /// A request the route accepts, plus one field the description does not name.
    UnknownField,
    /// A request the route accepts, with a `pin` that carries one field `Pin` does not name.
    UnknownPinField,
    /// `{tessera_id}` that is not a number.
    Identifier,
}

/// The ways `method path` can be malformed, each with the status a caller holding a valid token
/// gets for it. `/v1/meta` reads no query string, so its malformed query is served.
fn malformed_viewer_requests(method: &reqwest::Method, path: &str) -> Vec<(Malformed, u16)> {
    let mut kinds = Vec::new();
    if *method == reqwest::Method::GET {
        let status = if path == "/v1/meta" { 200 } else { 422 };
        kinds.push((Malformed::Query, status));
        kinds.push((Malformed::UnknownQuery, status));
    } else {
        kinds.extend([
            (Malformed::Syntax, 400),
            (Malformed::ContentType, 415),
            (Malformed::Shape, 422),
            (Malformed::UnknownField, 422),
        ]);
        if matches!(path, "/v1/viewport" | "/v1/items/{tessera_id}") {
            kinds.push((Malformed::UnknownPinField, 422));
        }
    }
    if path.contains("{tessera_id}") {
        kinds.push((Malformed::Identifier, 400));
    }
    kinds
}

/// Send `method path` to the viewer listener, malformed as `kind`, with `credential` as its bearer.
/// A path parameter is `1`, which satisfies both `{tessera_id}` and `{column}` and need not name
/// anything.
async fn send_viewer_probe(
    server: &TestServer,
    method: &reqwest::Method,
    path: &str,
    kind: Malformed,
    credential: Option<&str>,
) -> reqwest::Response {
    let concrete = path
        .split('/')
        .map(|s| match s {
            "{tessera_id}" if kind == Malformed::Identifier => "not-a-number",
            s if s.starts_with('{') => "1",
            s => s,
        })
        .collect::<Vec<_>>()
        .join("/");
    let url = match kind {
        Malformed::Query => server.viewer_url(&format!("{concrete}?limit=not-a-number")),
        Malformed::UnknownQuery => server.viewer_url(&format!("{concrete}?unknown_param=1")),
        _ => server.viewer_url(&concrete),
    };
    let mut req = server.client.request(method.clone(), url);
    if let Some(credential) = credential {
        req = req.bearer_auth(credential);
    }
    if *method == reqwest::Method::POST {
        req = with_body(req, kind, &viewer_body(path));
    }
    req.send().await.unwrap()
}

/// `req` carrying `body` as JSON, or malformed as `kind`.
fn with_body(
    req: reqwest::RequestBuilder,
    kind: Malformed,
    body: &Value,
) -> reqwest::RequestBuilder {
    match kind {
        Malformed::Syntax => req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{"),
        Malformed::ContentType => req
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(body.to_string()),
        Malformed::Shape => req.json(&json!("not a request object")),
        Malformed::UnknownField => {
            let mut body = body.clone();
            body["unknown_field"] = json!(1);
            req.json(&body)
        }
        Malformed::UnknownPinField => {
            let mut body = body.clone();
            body["pin"] = json!({ "prefix": "p", "segments_version": 0, "unknown_field": 1 });
            req.json(&body)
        }
        _ => req.json(body),
    }
}

/// **Both session-plane routes refuse a caller without an accepted API key before they read the
/// body.** With no credential or a wrong one, a request the route accepts and each malformed
/// body answer 401, so an unauthenticated caller learns nothing about the request shape and no
/// body is buffered for them. With the credential, the malformed bodies answer the refusal the
/// route gives them, so the 401s are not the route refusing everything.
#[tokio::test]
async fn both_session_routes_require_the_credential_before_the_body() {
    let doc = description();
    let f = fixture().await;
    let routes = [
        ("/session/authorise", json!({ "principal": principal_for(&f.server, &["0"]) })),
        ("/session/revoke", json!({ "token_id": 1 })),
    ];
    let malformed = [
        (Malformed::Syntax, 400),
        (Malformed::ContentType, 415),
        (Malformed::Shape, 422),
        (Malformed::UnknownField, 422),
    ];
    for (path, body) in &routes {
        let url = f.server.session_url(path);
        let kinds = std::iter::once(Malformed::No).chain(malformed.iter().map(|(kind, _)| *kind));
        for kind in kinds {
            for credential in [None, Some("not-a-key"), Some("tsk_nope_nope")] {
                let mut req = f.server.client.post(url.clone());
                if let Some(credential) = credential {
                    req = req.bearer_auth(credential);
                }
                let resp = with_body(req, kind, body).send().await.unwrap();
                assert_eq!(
                    resp.status().as_u16(),
                    401,
                    "POST {path} ({kind:?}) with credential {credential:?}"
                );
                assert_refusal(&doc, resp, 401, "bad-credential").await;
            }
        }
        for (kind, status) in malformed {
            let req = f
                .server
                .client
                .post(url.clone())
                .bearer_auth(&f.server.integrator_key);
            let resp = with_body(req, kind, body).send().await.unwrap();
            if matches!(kind, Malformed::Shape | Malformed::UnknownField) {
                assert_refusal(&doc, resp, status, "contract").await;
            } else {
                assert_eq!(
                    resp.status().as_u16(),
                    status,
                    "POST {path} ({kind:?}) with the session credential"
                );
            }
        }
    }
}

/// A body each POST route on the viewer plane accepts. A described POST route with no entry here
/// is a panic naming the path, so a new route is not probed with a body it would refuse anyway.
fn viewer_body(path: &str) -> Value {
    match path {
        "/v1/viewport" => viewport_body(json!({})),
        "/v1/items" => json!({ "view": "s0", "fields": [] }),
        "/v1/items/{tessera_id}" => json!({}),
        "/v1/artifacts" => json!({ "view": "s0", "layer": LAYER, "fields": [] }),
        "/v1/artifacts/{tessera_id}" => json!({ "view": "s0" }),
        "/v1/artifacts/browse" => json!({ "view": "s0", "layer": "clusters/none" }),
        "/v1/categories/{column}/suggest" => json!({ "q": "a" }),
        "/v1/aggregate" => json!({ "view": "s0", "groupings": [{}] }),
        "/v1/logout" => json!({}),
        other => panic!(
            "{other} is a described POST route and this test has no request body for it; add \
             one rather than letting a new viewer route go unchecked"
        ),
    }
}

/// **An underlay request that yields no cells carries a present, zero-row kind-2 frame — never no
/// frame at all** (contracts §3.2 item 2: presence follows the request, not the result).
///
/// The two states are what a positional reader tells apart, and only one of them is otherwise
/// tested: [`viewport_carries_the_described_headers_and_framing`] asserts `is_some()` where the
/// underlay returns cells and `is_none()` where none was asked for. The empty middle — asked for,
/// nothing to say — is asserted here, over a bbox in a corner of the extent that holds no point.
///
/// Two non-vacuity guards, because a zero-row frame is what a broken underlay pass would also
/// produce: the *same* request shape over an occupied bbox is asserted to carry cells, and the
/// *same* empty bbox with `underlay_offset` dropped is asserted to carry no frame — so the frame
/// here is present because it was asked for, and empty because the bbox is.
///
/// **Mutations this kills:** narrowing `WireSink::counts`'s `if let Some(cells) = sub_cells` to
/// `sub_cells.filter(|c| !c.is_empty())`, or any other change that makes the frame's presence
/// follow the result — the tidy-up that reads as a saving and takes the kind-2 slot away exactly
/// when the answer is *no marks here*.
#[tokio::test]
async fn an_underlay_that_finds_no_cells_still_carries_a_zero_row_frame() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    // Zoom 15 puts a tile at ~0.03 extent units, so this square holds no point of the fixture —
    // the nearest are at (0, 0) and (37, 53).
    let empty = json!({
        "view": "s0", "zoom": 15, "bbox": [10.0, 10.0, 10.5, 10.5], "k": 200,
        "underlay_offset": 1,
    });
    assert_valid(&doc, "ViewportRequest", &empty);
    let resp = viewport(&f.server, token, &empty).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    // A tile with nothing visible carries no count row at all (`tessera-engine`'s `visible == 0`
    // skip), so the zero-cell underlay is necessarily a zero-tile response. That is the state
    // under test, and it is still a complete answer — a trailer, and no points.
    assert!(decoded.tiles.is_empty(), "nothing is visible in this bbox");
    assert_eq!(decoded.trailer["points"], json!(0));
    assert_eq!(
        decoded.sub_cells.as_deref(),
        Some(&[][..]),
        "an underlay asked for and yielding no cells is a present, zero-row frame — `None` here \
         would be no frame at all, which is the shape reserved for an underlay never requested"
    );

    // The frame's presence is `underlay_offset`'s doing and nothing else: the same request
    // without it carries no kind-2 frame, which is the state an absent frame is reserved for. Two
    // responses over the same empty bbox, distinguished only by the field that decides presence.
    let mut unasked = empty.clone();
    unasked.as_object_mut().unwrap().remove("underlay_offset");
    let resp = viewport(&f.server, token, &unasked).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(
        decoded.sub_cells.is_none(),
        "an underlay never requested is no frame at all, and is not the zero-row frame above"
    );

    // The positive control: the same request shape, at the same zoom and over a bbox of the
    // same order, where a point does sit — (0, 0). A wider bbox would not do: at zoom 15 the
    // whole extent is far past `max_tiles_per_request` and would be refused rather than served.
    let occupied = json!({
        "view": "s0", "zoom": 15, "bbox": [0.0, 0.0, 1.0, 1.0], "k": 200,
        "underlay_offset": 1,
    });
    let resp = viewport(&f.server, token, &occupied).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(
        decoded.sub_cells.is_some_and(|c| !c.is_empty()),
        "the underlay pass serves cells when there are cells to serve"
    );
}

// ---------------------------------------------------------------------------------------------
// The control plane.
// ---------------------------------------------------------------------------------------------

/// `method path` on the control listener, with the operator credential.
fn control(server: &TestServer, method: &reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    server
        .client
        .request(method.clone(), server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
}

const ARROW: &str = "application/vnd.apache.arrow.stream";

/// `POST /control/ingest` in both encodings, a replay, the refusals, and an edit setting a column
/// declared at the running service.
#[tokio::test]
async fn ingest_matches_the_description() {
    let doc = description();
    let f = fixture().await;
    let post = reqwest::Method::POST;
    let ingest = |batch_id: &str| {
        control(&f.server, &post, "/control/ingest").header("x-tessera-batch-id", batch_id)
    };

    // JSON, with a category and a number, and one row with no `id`.
    let rows = json!([
        { "id": 1001, "x": 10.0, "y": 20.0, "access": ["0"],
          "archive": "astro", "score": 1.5 },
        { "x": 30.0, "y": 40.0, "access": "0", "archive": "hep", "score": null },
    ]);
    assert_valid(&doc, "IngestRecords", &rows);
    let body = rows.to_string();
    let resp = ingest("openapi-json")
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let answer = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(answer["created"], 2);
    assert_eq!(answer["tessera_ids"].as_array().unwrap().len(), 2);
    assert!(answer.get("replayed").is_none() && answer.get("visible").is_none());

    // The same bytes again are a replay; different bytes under the same id are a conflict.
    let resp = ingest("openapi-json")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let replay = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["created"], 0);
    assert_eq!(replay["tessera_ids"], answer["tessera_ids"]);
    let resp = ingest("openapi-json")
        .json(&json!([]))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 409, "conflict").await;

    // Arrow leaving both declared columns out: the item is created with no value in either.
    let resp = control(&f.server, &post, "/control/ingest")
        .header("x-tessera-batch-id", "openapi-arrow")
        .header("content-type", ARROW)
        .body(build_ingest_batch_optional(&[(Some(1002), 50.0, 60.0, "0")]))
        .send()
        .await
        .unwrap();
    let created = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(created["created"], 1);

    // The row again, naming the item by its `tessera_id` alone, changes nothing; a row naming no
    // item and carrying no position creates nothing and is refused; a batch of such rows with no
    // column to name items by is `422`.
    let named = json!([{ "tessera_id": created["tessera_ids"][0] }]);
    assert_valid(&doc, "IngestRecords", &named);
    let resp = ingest("openapi-named")
        .header("content-type", "application/json")
        .body(named.to_string())
        .send()
        .await
        .unwrap();
    let unchanged = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(unchanged["unchanged"], 1);
    let resp = ingest("openapi-unaddressed")
        .header("content-type", "application/json")
        .body(json!([{ "access": ["0"] }]).to_string())
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    let unplaced = json!([{ "id": null, "access": ["0"] }]).to_string();
    let resp = ingest("openapi-unplaced")
        .header("content-type", "application/json")
        .body(unplaced.clone())
        .send()
        .await
        .unwrap();
    let refused = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(refused["created"], 0);
    assert_eq!(refused["tessera_ids"], json!([null]));
    assert_eq!(refused["refused"], json!([{ "row": 0, "reason": "names_no_item" }]));
    let resp = control(&f.server, &post, "/control/ingest?strict=true")
        .header("x-tessera-batch-id", "openapi-unplaced-strict")
        .header("content-type", "application/json")
        .body(unplaced)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 409, "conflict").await;

    // Waiting for the rows to be visible.
    let resp = control(&f.server, &post, "/control/ingest?wait=visible")
        .header("x-tessera-batch-id", "openapi-visible")
        .json(&json!([{ "id": 1003, "x": 50.0, "y": 60.0,
                         "access": "0", "archive": "astro", "score": null }]))
        .send()
        .await
        .unwrap();
    let answer = assert_answer(&doc, &post, resp, 200).await;
    assert!(answer["visible"].is_boolean());

    // Refusals: no batch id, an unknown view, an undeclared column, an encoding the route does
    // not take, and no credential.
    let one = json!([{ "x": 1.0, "y": 1.0, "access": ["0"] }]);
    let resp = control(&f.server, &post, "/control/ingest")
        .json(&one)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    let resp = ingest("openapi-view")
        .header("x-tessera-view", "no-such-view")
        .json(&one)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 404, "unknown").await;
    let resp = ingest("openapi-column")
        .json(&json!([{ "x": 1.0, "y": 1.0, "access": ["0"], "no_such_column": 1 }]))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    let resp = ingest("openapi-csv")
        .header("content-type", "text/csv")
        .body("x,y\n1,1\n")
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    let resp = f
        .server
        .client
        .post(f.server.control_url("/control/ingest"))
        .header("x-tessera-batch-id", "openapi-anonymous")
        .json(&one)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 401, "bad-credential").await;

    // A row without coordinates sets a column declared while the service runs, on an item the
    // build placed, and edits it.
    let put = reqwest::Method::PUT;
    let resp = control(&f.server, &put, "/control/attributes")
        .json(&json!({ "name": "rating", "type": "f64" }))
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &put, resp, 201).await;
    let rows = json!([{ "id": 3, "rating": 4.5 }]);
    assert_valid(&doc, "IngestRecords", &rows);
    let resp = ingest("openapi-edit").json(&rows).send().await.unwrap();
    let answer = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(
        (answer["rows"].clone(), answer["edited"].clone()),
        (json!(1), json!(1))
    );
}

/// A built item's `tessera_id`, as a string: the first point a viewport serves.
async fn answer_id(server: &TestServer) -> String {
    let token = token_for(server, &["0"]).await;
    let resp = viewport(server, &token, &viewport_body(json!({}))).await;
    let (tessera_id, _) = decode_viewport_frames(&resp.bytes().await.unwrap()).points[0];
    tessera_id.to_string()
}

/// `POST /control/changes`: each op, both address forms, and the refusals, none of which applies
/// anything.
#[tokio::test]
async fn changes_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let post = reqwest::Method::POST;
    let changes = |body: &Value| {
        control(&f.server, &post, "/control/changes")
            .json(body)
            .send()
    };

    let tessera_id = answer_id(&f.server).await;
    let body = json!([
        { "op": "suppress", "match": { "id": member(5) } },
        { "op": "suppress", "match": { "tessera_id": tessera_id } },
    ]);
    for item in body.as_array().unwrap() {
        assert_valid(&doc, "ChangeItem", item);
    }
    let resp = changes(&body).await.unwrap();
    assert_answer(&doc, &post, resp, 200).await;
    let resp = control(&f.server, &post, "/control/changes?wait=visible")
        .json(&json!([{ "op": "unsuppress", "match": { "id": member(5) } },
                      { "op": "delete", "match": { "id": member(6) } }]))
        .send()
        .await
        .unwrap();
    let answer = assert_answer(&doc, &post, resp, 200).await;
    assert!(answer["visible"].is_boolean());

    // A change naming no item is listed as refused, or refuses a strict request.
    let nothing = json!([{ "op": "suppress", "match": { "id": "1000000" } }]);
    let resp = changes(&nothing).await.unwrap();
    let answer = assert_answer(&doc, &post, resp, 200).await;
    assert_eq!(answer["accepted"], 0, "{answer}");
    assert_eq!(answer["refused"], json!([{ "row": 0, "reason": "names_no_item" }]));
    let resp = control(&f.server, &post, "/control/changes?strict=true")
        .json(&nothing)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 404, "unknown").await;

    // Refusals. The schema refuses the shapes the server refuses.
    for item in [
        json!({ "op": "predicate", "match": { "id": member(5) } }),
        json!({ "op": "suppress", "match": { "id": member(5) }, "tessera_id": tessera_id }),
        json!({ "field": "id", "value": member(5), "op": "suppress" }),
        json!({ "op": "suppress" }),
        json!({ "op": "suppress", "match": { "tessera_id": 12345 } }),
        json!({ "op": "suppress", "match": { "id": member(5) }, "unknown": 1 }),
    ] {
        assert_invalid(&doc, "ChangeItem", &item);
        let resp = changes(&json!([item])).await.unwrap();
        assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    }
    // The schema takes any value under a key, since it cannot tell a unique field from a key
    // naming nothing; the server refuses a list in a unique field's column.
    let listed = json!([{ "op": "suppress", "match": { "id": [member(5)] } }]);
    let resp = changes(&listed).await.unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
    let resp = changes(&json!({ "op": "suppress" })).await.unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;
}

/// `GET /control/status`, `POST /control/flush` and `POST /control/compact`.
#[tokio::test]
async fn status_flush_and_compact_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let (get, post) = (reqwest::Method::GET, reqwest::Method::POST);

    let resp = control(&f.server, &get, "/control/status")
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &get, resp, 200).await;

    let resp = control(&f.server, &post, "/control/flush")
        .send()
        .await
        .unwrap();
    let answer = assert_answer(&doc, &post, resp, 202).await;
    assert!(answer.get("visible").is_none());
    let resp = control(&f.server, &post, "/control/flush?wait=visible")
        .send()
        .await
        .unwrap();
    let answer = assert_answer(&doc, &post, resp, 202).await;
    assert_eq!(answer["visible"], true);
    let resp = control(&f.server, &post, "/control/flush?wait=soon")
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&post), resp, 422, "contract").await;

    let resp = control(&f.server, &post, "/control/compact")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    assert!(described_responses(&doc, Some(&post), "/control/compact")
        .get("202")
        .is_some());
    assert!(
        resp.bytes().await.unwrap().is_empty(),
        "a compact's 202 has no body"
    );

    // After a flush has published, the status still matches.
    let resp = control(&f.server, &get, "/control/status")
        .send()
        .await
        .unwrap();
    let status = assert_answer(&doc, &get, resp, 200).await;
    assert!(status["publication"].as_u64().unwrap() >= answer["publication"].as_u64().unwrap());
    let resp = f
        .server
        .client
        .get(f.server.control_url("/control/status"))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&get), resp, 401, "bad-credential").await;
}

/// The declaration routes: attributes, vocabularies and their value pages, view groups and the
/// views created in them, and plain views. Each answers `201` for a new name and `200` for the
/// same declaration again, except a group's view, whose key is taken once.
#[tokio::test]
async fn declarations_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let (put, patch, delete) = (
        reqwest::Method::PUT,
        reqwest::Method::PATCH,
        reqwest::Method::DELETE,
    );
    let send = |method: &reqwest::Method, path: &str, body: &Value| {
        control(&f.server, method, path).json(body).send()
    };

    // Attributes.
    let attribute = json!({ "name": "rating", "title": "Rating", "type": "f64", "index": true });
    assert_valid(&doc, "AttributeDeclaration", &attribute);
    let resp = send(&put, "/control/attributes", &attribute).await.unwrap();
    assert_eq!(
        assert_answer(&doc, &put, resp, 201).await["existing"],
        false
    );
    let resp = send(&put, "/control/attributes?wait=visible", &attribute)
        .await
        .unwrap();
    let answer = assert_answer(&doc, &put, resp, 200).await;
    assert_eq!(answer["existing"], true);
    assert!(answer["visible"].is_boolean());
    let resp = send(
        &put,
        "/control/attributes",
        &json!({ "name": "rating", "type": "f32" }),
    )
    .await
    .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 409, "conflict").await;
    for refused in [
        json!({ "name": "flag", "type": "bool", "render": true }),
        json!({ "name": "flag", "type": "bool", "unknown": 1 }),
        json!({ "name": "flag", "type": "category" }),
    ] {
        let resp = send(&put, "/control/attributes", &refused).await.unwrap();
        assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    }
    assert_invalid(
        &doc,
        "AttributeDeclaration",
        &json!({ "name": "flag", "type": "string" }),
    );
    let resp = control(&f.server, &put, "/control/attributes")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_framework_refusal(&doc, &put, resp, 400).await;
    let resp = control(&f.server, &put, "/control/attributes")
        .body(attribute.to_string())
        .send()
        .await
        .unwrap();
    assert_framework_refusal(&doc, &put, resp, 415).await;
    let resp = control(&f.server, &put, "/control/attributes")
        .header("content-type", "application/json")
        .body(format!("{{\"name\": \"{}\"}}", "x".repeat(3 << 20)))
        .send()
        .await
        .unwrap();
    assert_framework_refusal(&doc, &put, resp, 413).await;

    // Vocabularies, and a page of values.
    let vocabulary = json!({ "title": "Genre", "value_set": "closed", "visibility": "public",
                             "width": "u8", "values": [{ "key": "drama" }], "reserved": [7] });
    assert_valid(&doc, "VocabularyDeclaration", &vocabulary);
    let resp = send(&put, "/control/vocabularies/genre", &vocabulary)
        .await
        .unwrap();
    assert_eq!(assert_answer(&doc, &put, resp, 201).await["added"], 1);
    let resp = send(&put, "/control/vocabularies/genre", &vocabulary)
        .await
        .unwrap();
    assert_eq!(assert_answer(&doc, &put, resp, 200).await["existing"], true);
    let mut wider = vocabulary.clone();
    wider["width"] = json!("u16");
    let resp = send(&put, "/control/vocabularies/genre", &wider)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 409, "conflict").await;
    let mut coded = vocabulary.clone();
    coded["values"] = json!([{ "key": "drama", "code": 3 }]);
    assert_invalid(&doc, "VocabularyDeclaration", &coded);
    let resp = send(&put, "/control/vocabularies/genre", &coded)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;

    let page = json!({ "values": [{ "key": "comedy", "title": "Comedy" },
                                  { "key": "drama", "title": "Drama" }] });
    assert_valid(&doc, "VocabularyValues", &page);
    let resp = send(&patch, "/control/vocabularies/genre/values", &page)
        .await
        .unwrap();
    let answer = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(
        (
            answer["added"].clone(),
            answer["existing"].clone(),
            answer["titles"].clone()
        ),
        (json!(1), json!(1), json!(1))
    );
    let resp = send(&patch, "/control/vocabularies/no-such/values", &page)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&patch), resp, 404, "unknown").await;
    let resp = send(
        &patch,
        "/control/vocabularies/genre/values",
        &json!({ "values": [{ "key": "" }] }),
    )
    .await
    .unwrap();
    assert_refusal_to(&doc, Some(&patch), resp, 422, "contract").await;

    // A view group, and the views created in it.
    let group = json!({ "title": "Quarters", "projection": "none",
                        "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] },
                        "visibility": "0", "point_visibility": { "default": "0" },
                        "metadata": [{ "name": "year", "type": "i32" }] });
    assert_valid(&doc, "ViewGroupDeclaration", &group);
    let resp = send(&put, "/control/view_groups/quarter", &group)
        .await
        .unwrap();
    assert_eq!(
        assert_answer(&doc, &put, resp, 201).await["group"],
        "quarter"
    );
    let resp = send(&put, "/control/view_groups/quarter", &group)
        .await
        .unwrap();
    assert_eq!(assert_answer(&doc, &put, resp, 200).await["existing"], true);
    let mut moved = group.clone();
    moved["extent"]["x"] = json!([0.0, 500.0]);
    let resp = send(&put, "/control/view_groups/quarter", &moved)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 409, "conflict").await;
    let sharing = json!({ "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] },
                          "members": "no-such-group" });
    let resp = send(&put, "/control/view_groups/sharing", &sharing)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 404, "unknown").await;

    let record = json!({ "metadata": { "year": 2026 } });
    assert_valid(&doc, "ViewRecord", &record);
    let resp = send(&put, "/control/views/quarter/2026-Q3", &record)
        .await
        .unwrap();
    assert_eq!(
        assert_answer(&doc, &put, resp, 201).await["view"],
        "quarter:2026-Q3"
    );
    let resp = send(&put, "/control/views/quarter/2026-Q3", &record)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 409, "conflict").await;
    let resp = send(&put, "/control/views/no-such-group/2026-Q3", &record)
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 404, "unknown").await;
    let resp = send(&put, "/control/views/quarter/2026-Q4", &json!({}))
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;

    let resp = control(&f.server, &delete, "/control/views/quarter/2026-Q3")
        .send()
        .await
        .unwrap();
    assert_eq!(assert_answer(&doc, &delete, resp, 200).await["deleted"], 0);
    let resp = control(&f.server, &delete, "/control/views/quarter/2026-Q3")
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&delete), resp, 404, "unknown").await;
    let resp = control(&f.server, &delete, "/control/views/quarter/2026-Q3?cascade=true")
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&delete), resp, 422, "contract").await;

    // A plain view, whose name shares a namespace with the groups'.
    let plain = json!({ "title": "Second layout",
                        "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] } });
    assert_valid(&doc, "PlainViewDeclaration", &plain);
    let resp = send(&put, "/control/views/s1", &plain).await.unwrap();
    assert_eq!(assert_answer(&doc, &put, resp, 201).await["view"], "s1");
    let resp = send(&put, "/control/views/s1", &plain).await.unwrap();
    assert_eq!(assert_answer(&doc, &put, resp, 200).await["existing"], true);
    let mut moved = plain.clone();
    moved["extent"]["y"] = json!([0.0, 10.0]);
    let resp = send(&put, "/control/views/s1", &moved).await.unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 409, "conflict").await;
    let resp = send(&put, "/control/views/quarter", &plain).await.unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    assert_invalid(
        &doc,
        "PlainViewDeclaration",
        &json!({ "title": "no extent" }),
    );
}

/// An Arrow growth page: `key` and a `members` table per row.
fn grow_arrow(key: &str, members: &Value) -> Vec<u8> {
    use arrow::array::Array;
    use arrow::datatypes::{DataType, Field, Schema};
    let lists = arrow_member_lists(std::slice::from_ref(members));
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("members", lists.data_type().clone(), true),
    ]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(StringArray::from_iter_values([key])),
            std::sync::Arc::new(lists),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// A layer registered, published into, grown in both encodings and dropped, with the refusals of
/// each step.
#[tokio::test]
async fn layers_and_artifacts_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let (put, patch, delete) = (
        reqwest::Method::PUT,
        reqwest::Method::PATCH,
        reqwest::Method::DELETE,
    );
    const NAME: &str = "clusters/b";
    let artifacts = format!("/control/layers/{}/artifacts", NAME.replace('/', "%2F"));

    // The least a declaration states: every optional key left out.
    let declaration = json!({
        "name": NAME,
        "views": ["s0"],
        "membership": "enumerated",
        "artifact_visibility": { "default": "inherited" },
        "hierarchy": { "kind": "flat" },
    });
    assert_valid(&doc, "LayerDeclaration", &declaration);
    let resp = control(&f.server, &put, "/control/layers")
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let registered = assert_answer(&doc, &put, resp, 201).await;
    assert_eq!(registered["name"], NAME);
    // A name is registered once, whatever the declaration.
    let resp = control(&f.server, &put, "/control/layers")
        .json(&declaration)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    let mut elsewhere = declaration.clone();
    elsewhere["name"] = json!("clusters/c");
    elsewhere["views"] = json!(["no-such-view"]);
    let resp = control(&f.server, &put, "/control/layers")
        .json(&elsewhere)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    let mut removed = elsewhere.clone();
    removed["views"] = json!(["s0"]);
    removed["content"] = json!({ "computed": [], "withdraw_on_member_deletion": true });
    assert_invalid(&doc, "LayerDeclaration", &removed);
    let resp = control(&f.server, &put, "/control/layers")
        .json(&removed)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;

    // A publication, and the same again, which creates nothing.
    let publication = json!({ "artifacts": [{ "key": "k0", "members": members(0..4) }] });
    assert_valid(&doc, "PublishRequest", &publication);
    let resp = control(&f.server, &put, &artifacts)
        .json(&publication)
        .send()
        .await
        .unwrap();
    let published = assert_answer(&doc, &put, resp, 201).await;
    assert_eq!(published["created"], 1);
    let resp = control(&f.server, &put, &format!("{artifacts}?wait=visible"))
        .json(&publication)
        .send()
        .await
        .unwrap();
    let again = assert_answer(&doc, &put, resp, 200).await;
    assert_eq!(again["created"], 0);
    assert_eq!(again["artifacts"], published["artifacts"]);

    // A member that names nothing is left out and listed, or refuses a strict request.
    let unnamed = json!({ "artifacts": [{ "key": "k1", "members": members([1_000_000]) }] });
    let resp = control(&f.server, &put, &format!("{artifacts}?strict=true"))
        .json(&unnamed)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 404, "unknown").await;
    let resp = control(&f.server, &put, &artifacts)
        .json(&unnamed)
        .send()
        .await
        .unwrap();
    let left_out = assert_answer(&doc, &put, resp, 201).await;
    assert_eq!(
        left_out["refused"],
        json!([{ "artifact": 0, "list": "members", "row": 0, "reason": "names_no_item" }])
    );

    // Refusals: Arrow, no artifacts, and a key the envelope lacks.
    let resp = control(&f.server, &put, &artifacts)
        .header("content-type", ARROW)
        .body(grow_arrow("k0", &members(4..6)))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    let empty = json!({ "artifacts": [] });
    let addressed =
        json!({ "addressing": "field", "artifacts": [{ "key": "k2", "members": members(0..1) }] });
    let fielded =
        json!({ "field": "id", "artifacts": [{ "key": "k2", "members": members(0..1) }] });
    let listed = json!({ "artifacts": [{ "key": "k2", "members": [member(0)] }] });
    for body in [empty, addressed, fielded, listed] {
        assert_invalid(&doc, "PublishRequest", &body);
        let resp = control(&f.server, &put, &artifacts)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_refusal_to(&doc, Some(&put), resp, 422, "contract").await;
    }

    // Growth, as JSON and as Arrow.
    let growth = json!({ "artifacts": [{ "key": "k0", "members": members(4..6) }] });
    assert_valid(&doc, "GrowRequest", &growth);
    let resp = control(&f.server, &patch, &artifacts)
        .json(&growth)
        .send()
        .await
        .unwrap();
    let grown = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(grown["artifacts"][0]["joined"], 2);
    let resp = control(&f.server, &patch, &artifacts)
        .header("content-type", ARROW)
        .body(grow_arrow("k0", &members(6..9)))
        .send()
        .await
        .unwrap();
    let grown = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(grown["artifacts"][0]["joined"], 3);
    let resp = control(&f.server, &patch, &artifacts)
        .json(&json!({ "artifacts": [{ "key": "no-such-key", "members": members(0..1) }] }))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&patch), resp, 422, "contract").await;
    let unnamed = json!({ "artifacts": [{ "key": "k0", "members": members([1_000_000]) }] });
    let resp = control(&f.server, &patch, &format!("{artifacts}?strict=true"))
        .json(&unnamed)
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&patch), resp, 404, "unknown").await;
    let resp = control(&f.server, &patch, &artifacts)
        .json(&unnamed)
        .send()
        .await
        .unwrap();
    let left_out = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(left_out["artifacts"][0]["joined"], 0);
    assert_eq!(
        left_out["refused"],
        json!([{ "artifact": 0, "list": "members", "row": 0, "reason": "names_no_item" }])
    );

    // The drop, and a second one, which names no layer.
    let layer = format!("/control/layers/{}", NAME.replace('/', "%2F"));
    let resp = control(&f.server, &delete, &layer).send().await.unwrap();
    assert_answer(&doc, &delete, resp, 200).await;
    let resp = control(&f.server, &delete, &layer).send().await.unwrap();
    assert_refusal_to(&doc, Some(&delete), resp, 422, "contract").await;
}

/// A shape layer's publication and growth answer with `shapes`, a generating-set page that
/// empties its set answers with `withdrawn`, and both match the description. So do the artifact
/// routes' refusals of a body that is not JSON and of a path segment that is not UTF-8.
#[tokio::test]
async fn shape_reports_and_withdrawals_match_the_description() {
    let doc = description();
    let f = fixture().await;
    let (put, patch, delete) = (
        reqwest::Method::PUT,
        reqwest::Method::PATCH,
        reqwest::Method::DELETE,
    );

    // A spatial layer whose artifacts are boxes.
    let boxes = json!({
        "name": "regions/boxes",
        "views": ["s0"],
        "membership": "spatial",
        "shape": { "kind": "bbox" },
        "artifact_visibility": { "default": "inherited" },
        "hierarchy": { "kind": "flat" },
    });
    assert_valid(&doc, "LayerDeclaration", &boxes);
    let resp = control(&f.server, &put, "/control/layers")
        .json(&boxes)
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &put, resp, 201).await;
    let url = "/control/layers/regions%2Fboxes/artifacts";
    let publication = json!({ "artifacts": [{ "key": "west", "members": {},
                                              "bbox": [0.0, 0.0, 500.0, 1000.0] }] });
    assert_valid(&doc, "PublishRequest", &publication);
    let resp = control(&f.server, &put, url)
        .json(&publication)
        .send()
        .await
        .unwrap();
    let published = assert_answer(&doc, &put, resp, 201).await;
    let views = published["shapes"][0]["views"].as_array().unwrap();
    assert_eq!(
        views.len(),
        1,
        "one report per view of the layer: {published}"
    );
    assert_eq!(views[0]["view"], "s0");

    // A spatial layer's artifacts are not grown.
    let resp = control(&f.server, &patch, url)
        .json(&json!({ "artifacts": [{ "key": "west", "members": members(0..1) }] }))
        .send()
        .await
        .unwrap();
    assert_refusal_to(&doc, Some(&patch), resp, 422, "contract").await;

    // A growth that fills an authored circle reports the shape, naming the content's rank.
    let outlines = json!({
        "name": "outlines/a",
        "views": ["s0"],
        "membership": "enumerated",
        "artifact_visibility": { "default": "inherited" },
        "hierarchy": { "kind": "flat" },
        "content": { "supplied": [{ "name": "outline", "type": "circle",
                                    "require_member_visibility": "inherited" }] },
    });
    let resp = control(&f.server, &put, "/control/layers")
        .json(&outlines)
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &put, resp, 201).await;
    let url = "/control/layers/outlines%2Fa/artifacts";
    let resp = control(&f.server, &put, url)
        .json(&json!({ "artifacts": [{ "key": "o0", "members": members(0..5) }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        assert_answer(&doc, &put, resp, 201).await["without_content"],
        1
    );
    let fill = json!({ "artifacts": [{ "key": "o0",
                                       "content": [{ "rank": 0, "values": ["500 500 100"] }] }] });
    assert_valid(&doc, "GrowRequest", &fill);
    let resp = control(&f.server, &patch, url)
        .json(&fill)
        .send()
        .await
        .unwrap();
    let grown = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(grown["shapes"][0]["key"], "o0");
    assert_eq!(grown["shapes"][0]["content"], 0);

    // A layer whose content is served only to a viewer who sees all it was made from.
    let topics = json!({
        "name": "topics/a",
        "views": ["s0"],
        "membership": "enumerated",
        "artifact_visibility": { "default": "inherited" },
        "hierarchy": { "kind": "flat" },
        "content": { "supplied": [{ "name": "topic", "type": "text",
                                    "require_member_visibility": "all" }] },
    });
    assert_valid(&doc, "LayerDeclaration", &topics);
    let resp = control(&f.server, &put, "/control/layers")
        .json(&topics)
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &put, resp, 201).await;
    let url = "/control/layers/topics%2Fa/artifacts";
    let resp = control(&f.server, &put, url)
        .json(&json!({ "artifacts": [{ "key": "t0", "members": members(0..10),
                                       "content": [{ "values": ["a topic"],
                                                     "generated_from": members(0..4) }] }] }))
        .send()
        .await
        .unwrap();
    assert_answer(&doc, &put, resp, 201).await;
    let page = json!({ "artifacts": [{ "key": "t0", "rank": 0, "leaving": members(0..4) }] });
    assert_valid(&doc, "GrowRequest", &page);
    let resp = control(&f.server, &patch, url)
        .json(&page)
        .send()
        .await
        .unwrap();
    let grown = assert_answer(&doc, &patch, resp, 200).await;
    assert_eq!(grown["artifacts"][0]["left"], 4);
    assert_eq!(grown["artifacts"][0]["withdrawn"], 0);

    // The artifact routes read their own JSON, so a body that does not parse is the envelope's
    // `422`; a path segment that is not UTF-8 is the framework's `400`.
    for method in [&put, &patch] {
        let resp = control(&f.server, method, url)
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap();
        assert_refusal_to(&doc, Some(method), resp, 422, "contract").await;
        let resp = control(&f.server, method, "/control/layers/%FF/artifacts")
            .json(&page)
            .send()
            .await
            .unwrap();
        assert_framework_refusal(&doc, method, resp, 400).await;
    }
    let resp = control(&f.server, &delete, "/control/layers/%FF")
        .send()
        .await
        .unwrap();
    assert_framework_refusal(&doc, &delete, resp, 400).await;
}
