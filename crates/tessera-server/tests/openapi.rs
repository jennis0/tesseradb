//! **The OpenAPI description is kept true here, or it is not true.**
//!
//! `docs/openapi/tessera.yaml` is hand-authored — the JSON DTOs live in this crate, some private,
//! two responses built with `json!`, and the wire structs carry a deliberate *no serde derive*
//! (I10) — so nothing generates it from the types and nothing but this test stops it drifting.
//! Every route the viewer and session planes mount is exercised, with a success and at least one
//! refusal each, and **every JSON request and response body is validated against the
//! description's own schemas**. The closed DTOs are declared `additionalProperties: false`, so a
//! field added to a response and not to the document fails here rather than being discovered by
//! a stranger reading the wire.
//!
//! What this test does not do: decode the framed-Arrow body of `/v1/viewport` against a schema —
//! there is none to write, and the frame layout is contracts §5, checked by
//! [`common::decode_viewport_frames`] and by the two worked decodes under `clients/ts/wire-example`
//! and `reference/examples`. It asserts the route's headers and its framing outcomes (a `k = 0`
//! request yields no points frame; `layers: []` yields no artifacts frame) and validates the
//! request body only.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};
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

/// The error envelope on a refusal: the status the description declares, one closed code, and
/// — on a 429 — `Retry-After` agreeing with the body. Returns the body for further asserts.
async fn assert_refusal(doc: &Value, resp: reqwest::Response, status: u16, code: &str) -> Value {
    assert_eq!(resp.status().as_u16(), status);
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let body: Value = resp
        .json()
        .await
        .expect("a refusal carries the JSON envelope");
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
vocabulary = "archive"

[[attribute]]
name     = "score"
type     = "f32"
render   = true
"#;

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, false),
        Field::new("score", DataType::Float32, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<&str> = ids
        .iter()
        .map(|&e| ["astro", "cond", "hep"][(e % 3) as usize])
        .collect();
    let scores: Vec<Option<f32>> = ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(Float32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_fixture_with_schema(out: &Path, points: &Path, pairs: &Path) {
    write_points(points);
    write_pairs_n(pairs, N);
    let schema_path = points.with_file_name("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .unwrap()
        .schema;
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("fixture build should succeed");
}

fn build_bundle(tmp: &TempDir) -> std::path::PathBuf {
    let bundle_root = tmp.path().join("bundle");
    build_fixture_with_schema(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    bundle_root
}

const LAYER: &str = "clusters/a";

/// A flat, enumerated layer over the fixture's external ids, holding two artifacts: one every
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
        "content": { "computed": ["centroid", "hull", "box"], "supplied": [], "withdraw_on_member_deletion": true },
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

    let member = |source_id: u64| {
        base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
    };
    let members_all: Vec<String> = (0..12u64).map(member).collect();
    let members_broad: Vec<String> = (12..24u64).filter(|s| s % 3 != 0).map(member).collect();
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            LAYER.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
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
    let bundle_root = build_bundle(&tmp);
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
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
    let auth_data =
        base64::engine::general_purpose::STANDARD.encode(json!({ "terms": terms }).to_string());
    let body = json!({ "auth_data": auth_data });
    assert_valid(doc, "AuthoriseRequest", &body);
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(SESSION_CREDENTIAL)
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

/// The route set is `viewer::router` plus `session::router`, by hand — a route mounted and not
/// described, or described and not mounted, fails here.
#[test]
fn the_description_names_every_route_on_the_two_planes_and_no_other() {
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
            "/healthz",
            "/readyz",
            "/session/authorise",
            "/session/revoke",
            "/v1/artifacts/{tessera_id}",
            "/v1/categories/{column}",
            "/v1/items/{tessera_id}",
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
        "Meta",
        "DeclaredScalar",
        "Selection",
        "Layer",
        "CategoriesResponse",
        "Pin",
        "ViewportRequest",
        "ItemRequest",
        "ItemResponse",
        "ArtifactRequest",
        "ArtifactResponse",
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

/// The ruled `layers` semantics are in the schema: an array, or the literal string `"all"`, and
/// nothing else. The server in this tree does not yet accept the string — see
/// [`an_omitted_layers_field_means_no_artifacts_frame`] — so this is the shape alone.
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

    // Refusals: the wrong session credential, and auth_data that is not base64.
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/authorise"))
        .bearer_auth("not-the-credential")
        .json(&json!({ "auth_data": "e30=" }))
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/authorise"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&json!({ "auth_data": "not base64!" }))
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 422, "contract").await;

    // Revoke: 204 with no body, and the token is then a 401 — the session ending, exactly as a
    // 403 would be (the obligations list's rule).
    let body = json!({ "token_id": token_id });
    assert_valid(&doc, "RevokeRequest", &body);
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
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
    assert_refusal(&doc, resp, 401, "bad-credential").await;

    // Revoke without the credential.
    let resp = f
        .server
        .client
        .post(f.server.session_url("/session/revoke"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_refusal(&doc, resp, 401, "bad-credential").await;
}

#[tokio::test]
async fn a_saturated_gate_sheds_authorise_with_the_described_429() {
    let doc = description();
    let tmp = TempDir::new().unwrap();
    let bundle_root = build_bundle(&tmp);
    // No slots at all: every admission is shed before any wait.
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        default_engine_config(),
        ComputeGate::new(0, 0, 250),
    )
    .await;
    let auth_data =
        base64::engine::general_purpose::STANDARD.encode(json!({ "terms": ["0"] }).to_string());
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&json!({ "auth_data": auth_data }))
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

/// **The ruled semantics of an omitted `layers`, at the wire.** Owner ruling 2026-08-25: omitted
/// or `[]` means *no* layers, the string `"all"` means every reachable layer. The server change
/// landed, and this test runs — it was written against the ruled behaviour before the server had
/// it, `#[ignore]`d with that reason, and enabled at integration.
///
/// What it pins that its siblings do not is the pair *at one principal in one fixture*: the same
/// broad token, the same request but for the field, absent giving no artifacts frame and `"all"`
/// giving both reachable artifacts. `viewport_membership.rs`'s
/// `omitted_layers_means_none_and_the_word_all_means_every_reachable_layer` pins the same ruling
/// against the engine's membership columns, and
/// [`the_layers_field_is_an_array_or_the_string_all`] pins the shape the description accepts.
#[tokio::test]
async fn an_omitted_layers_field_means_no_artifacts_frame() {
    let doc = description();
    let f = fixture().await;
    let auth = authorise_checked(&doc, &f.server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = viewport(&f.server, token, &viewport_body(json!({ "k": 0 }))).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(decoded.artifacts.is_none(), "omitted `layers` is no layers");

    let resp = viewport(
        &f.server,
        token,
        &viewport_body(json!({ "k": 0, "layers": "all" })),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert_eq!(
        decoded.artifacts.map(|a| a.len()),
        Some(2),
        "\"all\" is every reachable layer"
    );
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

    let body = json!({ "idset": FIXTURE_IDSET });
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
    assert!(
        item["external_id"].is_string(),
        "the fixture mints external ids"
    );

    // Refusals: an item nobody with zero terms may see is a 404 (identical to nonexistence); a
    // stale idset is a 409, decided before anything is looked up.
    let nobody = authorise_checked(&doc, &f.server, &[]).await;
    let resp = post(tessera_id, json!({}), nobody["token"].as_str().unwrap())
        .await
        .unwrap();
    assert_refusal(&doc, resp, 404, "unknown").await;
    let resp = post(tessera_id, json!({ "idset": FIXTURE_IDSET + 1 }), token)
        .await
        .unwrap();
    assert_refusal(&doc, resp, 409, "conflict").await;
    // The request schema refuses what the server refuses: an unknown field.
    assert_invalid(&doc, "ItemRequest", &json!({ "external_id": "x" }));
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
    // Same shape for a point's identifier, an unknown view, and a stale idset's 409.
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
    let resp = post(
        &f.artifacts[0],
        json!({ "view": "s0", "idset": FIXTURE_IDSET + 1 }),
        token,
    )
    .await
    .unwrap();
    assert_refusal(&doc, resp, 409, "conflict").await;
    // `view` is required by the schema, as by the server.
    assert_invalid(&doc, "ArtifactRequest", &json!({}));
}

/// **Every viewer-plane route refuses a caller with no session credential, and one whose
/// credential is not a token — enumerated from the description, not listed by hand.**
///
/// This is the viewer-plane counterpart of
/// `every_control_route_requires_the_operator_credential` (`tests/http_write.rs`), and it exists
/// for the same reason that test names: a per-route 401 test stays green forever while a sixth
/// route ships wide open. The control plane also has structural cover —
/// `require_operator_credential` wraps its whole router — and **the viewer plane has none**.
/// `viewer::router` mounts no credential layer at all; each handler opens with its own
/// `bearer_token(&headers).ok_or(ApiError::BadCredential)?`. A route written without that line is
/// caught by nothing but this test.
///
/// **What is enumerated, and why that is the router's own list.** axum 0.8 exposes no route
/// enumeration, so there is no way to ask the mounted router what it serves. The next-best source
/// is not a list in this file but `docs/openapi/tessera.yaml`, and the enumeration here is over the
/// description's operations and their declared `security` — so the assertion made is the
/// description's own claim, checked against the running server. The chain that makes it bite on a
/// *new* route is three links, all inside this crate:
/// [`the_description_names_every_route_on_the_two_planes_and_no_other`] fails if a route is mounted
/// and not described; describing it means declaring its `security`; and declaring `sessionToken`
/// puts it in this loop with no exemption to skip it. A route deliberately declared `security: []`
/// is skipped here — but that is an explicit published claim that it is unauthenticated, which is
/// a different thing from an oversight, and the two probes are asserted below to be the only ones.
///
/// **The requests carry well-formed bodies on purpose.** The `Json` extractor runs ahead of the
/// handler, so a POST with no body is refused during extraction and never reaches the credential
/// check — such a request would prove nothing about authentication. Each POST route therefore has
/// a body known to deserialise, and a route with no entry in that table is a panic naming the
/// path rather than a silent skip.
///
/// **Mutations this kills:** deleting the `bearer_token` line from any viewer handler (each is
/// followed by `authenticated_session`, so the pair must go, which is exactly the shape of a
/// handler written without either); mounting a new viewer route with no credential check;
/// answering a bare `StatusCode::UNAUTHORIZED` instead of `ApiError::BadCredential`'s body; and,
/// on the probe half, putting `/healthz` or `/readyz` behind the credential
/// (docs/decisions/0011-health-probes-off-control-plane.md).
#[tokio::test]
async fn every_viewer_route_requires_a_session_token() {
    let doc = description();
    let f = fixture().await;

    // A body each POST route deserialises, consulted only for POST. A described POST route with
    // no entry here is a panic naming the path, not a silent skip: without a body it accepts, the
    // request would be refused during extraction and would say nothing about its credential.
    let body_for = |path: &str| -> Value {
        match path {
            "/v1/viewport" => viewport_body(json!({})),
            "/v1/items/{tessera_id}" => json!({}),
            "/v1/artifacts/{tessera_id}" => json!({ "view": "s0" }),
            other => panic!(
                "{other} is a described POST route and this test has no request body for it; add \
                 one rather than letting a new viewer route go unchecked"
            ),
        }
    };

    // `{tessera_id}` is an `AxumPath<u64>` and `{column}` a `String`; `1` satisfies both, and the
    // credential check precedes any resolution of either, so the value need not exist.
    let concrete = |path: &str| -> String {
        path.split('/')
            .map(|s| if s.starts_with('{') { "1" } else { s })
            .collect::<Vec<_>>()
            .join("/")
    };

    let mut gated = 0usize;
    let mut probes = 0usize;
    let mut session_plane = 0usize;

    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let security = op["security"]
                .as_array()
                .unwrap_or_else(|| panic!("{method} {path} declares no `security`"));
            let scheme = security
                .first()
                .map(|s| s.as_object().unwrap().keys().next().unwrap().clone());
            let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let url = f.server.viewer_url(&concrete(path));

            match scheme.as_deref() {
                // The session plane's own credential, on its own listener — not this plane's
                // claim and not this test's (contracts §3.3).
                Some("sessionCredential") => {
                    session_plane += 1;
                    continue;
                }
                // Decision 0011: the probes answer without a credential, and they are the only
                // two routes on this plane that do.
                None => {
                    probes += 1;
                    let resp = f
                        .server
                        .client
                        .request(method.clone(), url.clone())
                        .send()
                        .await
                        .unwrap();
                    assert_ne!(
                        resp.status().as_u16(),
                        401,
                        "{method} {path} is described as unauthenticated and must not demand a \
                         credential"
                    );
                    continue;
                }
                Some("sessionToken") => gated += 1,
                Some(other) => panic!("{method} {path} declares an unknown scheme {other}"),
            }

            for credential in [None, Some("not-a-session-token")] {
                let mut req = f.server.client.request(method.clone(), url.clone());
                if let Some(credential) = credential {
                    req = req.bearer_auth(credential);
                }
                if method == reqwest::Method::POST {
                    req = req.json(&body_for(path));
                }
                let resp = req.send().await.unwrap();
                assert_eq!(
                    resp.status().as_u16(),
                    401,
                    "{method} {path} answered {} for credential {credential:?}; every viewer \
                     route, without exception, requires a session token",
                    resp.status()
                );
                let body = assert_refusal(&doc, resp, 401, "bad-credential").await;
                assert_eq!(
                    body["detail"], "missing or invalid bearer credential",
                    "{method} {path} must answer ApiError::BadCredential's own body, unchanged: \
                     {body}"
                );
            }
        }
    }

    // Non-vacuity, both halves: the loop must have found the five gated routes and the two
    // probes, or it enumerated nothing and proved nothing.
    assert_eq!(
        gated, 5,
        "the viewer plane's gated routes are meta, categories, viewport, items and artifacts; \
         a change to that set belongs in this test's reasoning, not silently in its count"
    );
    assert_eq!(
        probes, 2,
        "/healthz and /readyz are the only unauthenticated routes"
    );
    assert_eq!(
        session_plane, 2,
        "/session/authorise and /session/revoke are the session plane's"
    );
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
