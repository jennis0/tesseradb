//! **`tessera check --payloads`, posted** (`python-sdk.md` §6.2 step 1, §11.2 C; decision 0139):
//! every body the emitter produces goes to the route it names, over a running service, and is
//! taken.
//!
//! **This is the test that makes the emitter a contract rather than a shape.** The bodies are
//! assembled in `tessera_build::config::control_payloads` from the parsed declaration; whether
//! they are *payloads* is a question only the routes can answer, and a `deny_unknown_fields` body
//! that gained a key or lost one answers it with a 422 here. The per-key assertions — what is
//! dropped, what is kept — are `tessera-build`'s own `config::payload_tests`.

mod common;

use std::path::Path;

use common::*;
use serde_json::Value;
use tempfile::TempDir;

/// The items of the built bundle, which carries one view, `s0`, and nothing the declaration names:
/// every object below is one the running service created from a payload.
const ITEMS: u64 = 64;

/// The declaration these tests post: two plain views, a view group, two vocabularies (one with
/// inline values), three attributes and one layer.
///
/// **No `render` and no `auto`** — both are emitted as declared and both are refused at a running
/// service, which [`the_route_refuses_render_and_says_why`] pins separately. Everything else here
/// is a key some block takes, so a body that arrives wrong arrives wrong in this test.
const DECLARATION: &str = r#"
[sources]
points  = "points.parquet"
roster  = "roster.parquet"
members = "members.parquet"
shapes  = "shapes.parquet"

[defaults]
source          = "points"
allocation_view = "atlas"

[[view]]
name             = "atlas"
title            = "Atlas"
projection       = "web_mercator"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
point_visibility = { field = "access", default = "public" }

[[view]]
name             = "embedding"
title            = "Embedding"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { default = "public" }

[[view_group]]
name             = "quarter"
title            = "By quarter"
projection       = "none"
source           = "points"
fields           = { view = "quarter" }
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us" }

[view_group.views]
source = "roster"
fields = { key = "quarter" }

[[vocabulary]]
name       = "kind"
title      = "Feature kind"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = { alpha = 3, beta = 7 }
reserved   = [9]

[[vocabulary]]
name       = "mood"
title      = "Mood"
width      = "u16"
value_set  = "open"
visibility = "derived"

[[attribute]]
name            = "importance"
title           = "Importance"
field           = "pop"
entity_id_field = "entity_id"
type            = "u32"
index           = true

[[attribute]]
name       = "feature"
title      = "Feature class"
type       = "category"
vocabulary = "kind"
index      = true

[[attribute]]
name     = "note"
type     = "text"
analyser = "unicode"
index    = true

[[layer]]
name       = "clusters/a"
title      = "Clusters"
views      = ["atlas"]
source     = "shapes"
membership = "enumerated"
hierarchy  = { kind = "flat" }
visibility = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

[layer.members]
source = "members"
"#;

/// The emitted object over [`DECLARATION`] — the binary's own serialisation, reached through the
/// same function `tessera check --payloads` calls.
fn payloads(dir: &Path, declaration: &str) -> Value {
    let path = dir.join("declaration.toml");
    std::fs::write(&path, declaration).unwrap();
    let config = tessera_build::config::Config::parse(&path, &Default::default())
        .expect("the declaration should compile");
    tessera_build::config::control_payloads(&config)
}

async fn put(served: &Served, path: &str, body: &Value) -> (u16, Value) {
    send(served, reqwest::Method::PUT, path, body).await
}

async fn patch(served: &Served, path: &str, body: &Value) -> (u16, Value) {
    send(served, reqwest::Method::PATCH, path, body).await
}

async fn send(served: &Served, method: reqwest::Method, path: &str, body: &Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .request(method, served.server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `201` for a create and `200` for a restatement are both "the route took it"; anything else is
/// the emitter's body, since the declaration is one a build compiles.
fn accepted(status: u16, answer: &Value, what: &str) {
    assert!(status == 201 || status == 200, "{what}: {status} {answer}");
}

/// **Every emitted body, at the route it names.** The order is `commit()`'s (§6.2 step 1): a
/// vocabulary before the column that names it, a group before anything scoped to it, a view
/// before the layer drawn on it.
#[tokio::test]
async fn every_emitted_body_is_taken_by_its_route() {
    let served = Served::build(|dir| build_fixture(dir, ITEMS)).await;
    let dir = TempDir::new().unwrap();
    let payloads = payloads(dir.path(), DECLARATION);

    for entry in payloads["vocabularies"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let (status, answer) = put(
            &served,
            &format!("/control/vocabularies/{name}"),
            &entry["body"],
        )
        .await;
        accepted(status, &answer, &format!("vocabulary '{name}'"));
        // The values page, exactly as emitted: a body whose rows carried a code would be refused
        // here, which is what `deny_unknown_fields` on that route is for.
        if let Some(values) = entry.get("values") {
            let (status, answer) = patch(
                &served,
                &format!("/control/vocabularies/{name}/values"),
                values,
            )
            .await;
            assert_eq!(status, 200, "vocabulary '{name}' values: {answer}");
            // The declaration carried them, so the page is the join it is specified to be: every
            // key present and bound, nothing added.
            assert_eq!(answer["added"], 0, "{answer}");
        }
    }

    for entry in payloads["views"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let (status, answer) =
            put(&served, &format!("/control/views/{name}"), &entry["body"]).await;
        accepted(status, &answer, &format!("view '{name}'"));
    }

    for entry in payloads["view_groups"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let (status, answer) = put(
            &served,
            &format!("/control/view_groups/{name}"),
            &entry["body"],
        )
        .await;
        accepted(status, &answer, &format!("view group '{name}'"));
    }

    for body in payloads["attributes"].as_array().unwrap() {
        let name = body["name"].as_str().unwrap();
        let (status, answer) = put(&served, "/control/attributes", body).await;
        accepted(status, &answer, &format!("attribute '{name}'"));
    }

    for body in payloads["layers"].as_array().unwrap() {
        let name = body["name"].as_str().unwrap();
        let (status, answer) = put(&served, "/control/layers", body).await;
        accepted(status, &answer, &format!("layer '{name}'"));
    }

    // **Posted twice is the fill rule** (`ingest.md` §1.1): a payload the SDK re-sends after a
    // reconnect must read as present rather than as a conflict, so the second pass is 200.
    for entry in payloads["views"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let (status, answer) =
            put(&served, &format!("/control/views/{name}"), &entry["body"]).await;
        assert_eq!(status, 200, "view '{name}' restated: {answer}");
    }

    served.server.shutdown().await;
}

/// **The emitter states the declaration and the route decides** (decision 0136's amendment):
/// `render` reaches the body, and what comes back is the refusal that explains itself rather than
/// a column quietly declared without it.
#[tokio::test]
async fn the_route_refuses_render_and_says_why() {
    let served = Served::build(|dir| build_fixture(dir, ITEMS)).await;
    let dir = TempDir::new().unwrap();
    let payloads = payloads(
        dir.path(),
        r#"
[[view]]
name             = "s1"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { default = "public" }

[[attribute]]
name   = "importance"
type   = "u32"
render = true
"#,
    );
    let body = &payloads["attributes"][0];
    assert_eq!(body["render"], true, "emitted as declared: {body}");
    let (status, answer) = put(&served, "/control/attributes", body).await;
    assert_eq!(status, 422, "{answer}");
    assert_eq!(answer["error"], "contract", "{answer}");
    assert!(
        answer["detail"].as_str().unwrap_or_default().contains("render"),
        "the refusal names the key: {answer}"
    );
    served.server.shutdown().await;
}
