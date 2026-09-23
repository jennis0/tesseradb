//! An artifact's own access label, over the real routes.
//!
//! Two deployments are built alike. Deployment A also holds artifacts labelled `red`, published
//! after everything the two share so every shared artifact keeps its identifier, plus a parent edge
//! from a shared artifact to a red one and a label artifact attached to a red one. A viewer who
//! lacks `red` must get byte-identical answers from the two on every viewer route: that is what
//! "withheld means never published" asserts. A viewer holding `red` is the control, served what
//! the other is not. Neither `red` nor `blue` is carried by any point.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const LAYER: &str = "teams";
const NAMES: &str = "teams/names";
const WHOLE: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

fn artifacts_url(server: &TestServer, layer: &str) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        layer.replace('/', "%2F")
    ))
}

async fn declare(server: &TestServer, body: serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(status, 201, "{}", resp.text().await.unwrap());
}

fn teams_declaration(default: &str) -> serde_json::Value {
    let default = if default == "inherited" {
        json!("inherited")
    } else {
        json!({ "label": default })
    };
    json!({
        "name": LAYER,
        "title": LAYER,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": "team", "default": default },
        "require_member_visibility": null,
        "hierarchy": { "kind": "nested", "prune_children": false },
        "content": { "computed": ["centroid"], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
}

async fn publish(server: &TestServer, layer: &str, artifacts: serde_json::Value) -> serde_json::Value {
    let resp = server
        .client
        .put(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, 201, "{body}");
    body
}

async fn patch(server: &TestServer, layer: &str, artifacts: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .patch(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// The identifiers a publication answered, by key.
fn ids(body: &serde_json::Value) -> Vec<(String, String)> {
    body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["key"].as_str().unwrap().to_string(),
                a["tessera_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

struct Deployment {
    tmp: TempDir,
    server: TestServer,
    /// Every artifact identifier this deployment's publications answered, by key.
    ids: Vec<(String, String)>,
}

async fn open(tmp: &TempDir) -> TestServer {
    spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

/// One deployment. `red` adds the labelled artifacts, after everything the two share.
async fn deployment(red: bool) -> Deployment {
    let tmp = TempDir::new().unwrap();
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = open(&tmp).await;
    declare(&server, teams_declaration("inherited")).await;
    declare(
        &server,
        json!({
            "name": NAMES,
            "title": NAMES,
            "views": ["s0"],
            "membership": "enumerated",
            "value_set": "closed",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": {
                "computed": [],
                "supplied": [{ "name": "name", "type": "text", "require_member_visibility": "inherited" }]
            },
            "depends_on": [LAYER],
            "levels": []
        }),
    )
    .await;

    let mut all = ids(
        &publish(
            &server,
            LAYER,
            json!([
                { "key": "open", "members": members(0..40) },
                { "key": "p-open", "members": members(40..120) },
                { "key": "c-open", "members": members(0..20) },
                { "key": "shared", "members": members(120..160), "access": ["blue"] },
            ]),
        )
        .await,
    );
    all.extend(ids(
        &publish(
            &server,
            NAMES,
            json!([{ "key": "n-open", "attached_to": { "layer": LAYER, "key": "open" },
                     "content": [{ "values": ["Open"] }] }]),
        )
        .await,
    ));
    if red {
        all.extend(ids(
            &publish(
                &server,
                LAYER,
                json!([
                    { "key": "p-red", "members": members(0..60), "access": ["red"] },
                    { "key": "c-red", "members": members(40..60), "parent": ["p-open"],
                      "access": ["red"] },
                    { "key": "list-red", "members": members(200..260),
                      "access": ["green", "red"] },
                ]),
            )
            .await,
        ));
        let (status, body) =
            patch(&server, LAYER, json!([{ "key": "c-open", "parent": ["p-red"] }])).await;
        assert_eq!(status, 200, "{body}");
        all.extend(ids(
            &publish(
                &server,
                NAMES,
                json!([{ "key": "n-red", "attached_to": { "layer": LAYER, "key": "p-red" },
                         "content": [{ "values": ["Red"] }] }]),
            )
            .await,
        ));
    }
    tick(&server).await;
    Deployment {
        tmp,
        server,
        ids: all,
    }
}

async fn token(server: &TestServer, terms: &[&str]) -> String {
    authorise(server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn viewport_bytes(server: &TestServer, token: &str, body: serde_json::Value) -> Vec<u8> {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap().to_vec();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    decode_viewport_frames(&bytes).deterministic_bytes
}

async fn json_post(
    server: &TestServer,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .post(server.viewer_url(path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

fn viewport(zoom: u8, extra: serde_json::Value) -> serde_json::Value {
    let mut body = json!({
        "view": "s0",
        "zoom": zoom,
        "bbox": WHOLE,
        "k": 1000,
        "layers": "all",
        "artifact_budget": 1000,
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

fn keys_served(bytes: &[u8]) -> Vec<String> {
    let mut keys: Vec<String> = decode_viewport_frames(bytes)
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.key)
        .collect();
    keys.sort();
    keys
}

/// A raw viewport body, whole, for reading the artifacts frame.
async fn viewport_raw(server: &TestServer, token: &str, body: serde_json::Value) -> Vec<u8> {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.bytes().await.unwrap().to_vec()
}

/// Every viewer route, asked of both deployments by a principal lacking `red`, must answer alike.
/// Returns how many comparisons were made, so a caller can see the loop did not come up empty.
async fn assert_indistinguishable(a: &Deployment, b: &Deployment) -> usize {
    let blue_a = token(&a.server, &["0", "blue"]).await;
    let blue_b = token(&b.server, &["0", "blue"]).await;
    let red_ids: Vec<&(String, String)> = a
        .ids
        .iter()
        .filter(|(key, _)| !b.ids.iter().any(|(k, _)| k == key))
        .collect();
    assert_eq!(red_ids.len(), 4, "A holds four artifacts B does not");
    let mut compared = 0;

    // The viewport: the artifacts frame, parent links, targets and the points' membership
    // columns, at two depths.
    for zoom in [0u8, 3] {
        let body = viewport(zoom, json!({}));
        assert_eq!(
            viewport_bytes(&a.server, &blue_a, body.clone()).await,
            viewport_bytes(&b.server, &blue_b, body).await,
            "viewport at zoom {zoom}"
        );
        compared += 1;
    }

    // A filter and a highlight naming a shared artifact, and each naming a withheld one: the
    // withheld one is the empty operand a never-issued identifier is.
    let shared_open = &a.ids.iter().find(|(k, _)| k == "p-open").unwrap().1;
    let mut probes: Vec<String> = red_ids.iter().map(|(_, id)| id.clone()).collect();
    probes.push(shared_open.clone());
    for id in &probes {
        for leaf in [
            json!({ "member_of": { "layer": LAYER, "artifact": id } }),
            json!({ "region": { "artifact": id } }),
        ] {
            for field in ["filters", "highlight"] {
                let body = viewport(0, json!({ field: leaf }));
                assert_eq!(
                    viewport_bytes(&a.server, &blue_a, body.clone()).await,
                    viewport_bytes(&b.server, &blue_b, body).await,
                    "{field} {leaf}"
                );
                compared += 1;
            }
        }
    }

    // The identifier route, for every identifier A issued: B never issued the red ones.
    for (key, id) in &a.ids {
        let path = format!("/v1/artifacts/{id}");
        assert_eq!(
            json_post(&a.server, &blue_a, &path, json!({ "view": "s0" })).await,
            json_post(&b.server, &blue_b, &path, json!({ "view": "s0" })).await,
            "drill-down of {key}"
        );
        compared += 1;
    }

    // Browse: roots, the children of every identifier, and a search for every key.
    let browse = |extra: serde_json::Value| {
        let mut body = json!({ "view": "s0", "layer": LAYER, "limit": 100 });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };
    let mut asks = vec![browse(json!({}))];
    for (key, id) in &a.ids {
        asks.push(browse(json!({ "parent": id })));
        asks.push(browse(json!({ "q": key })));
    }
    asks.push(browse(json!({ "filters": { "member_of": { "layer": LAYER, "artifact": shared_open } } })));
    for ask in asks {
        assert_eq!(
            json_post(&a.server, &blue_a, "/v1/artifacts/browse", ask.clone()).await,
            json_post(&b.server, &blue_b, "/v1/artifacts/browse", ask.clone()).await,
            "browse {ask}"
        );
        compared += 1;
    }

    // Item cards, for a member of every red artifact.
    let points = decode_viewport_frames(
        &viewport_raw(&a.server, &blue_a, viewport(0, json!({ "layers": [] }))).await,
    )
    .points;
    for (tessera_id, _) in points.iter().take(40) {
        let a_item = post_item(&a.server, &blue_a, *tessera_id).await;
        let b_item = post_item(&b.server, &blue_b, *tessera_id).await;
        assert_eq!(a_item.status(), b_item.status());
        assert_eq!(
            a_item.text().await.unwrap(),
            b_item.text().await.unwrap(),
            "item card {tessera_id}"
        );
        compared += 1;
    }

    // What `/v1/meta` says about the layers.
    let meta = |server: &TestServer, token: String| {
        let request = server
            .client
            .get(server.viewer_url("/v1/meta"))
            .bearer_auth(token)
            .send();
        async move {
            let body: serde_json::Value = request.await.unwrap().json().await.unwrap();
            // A layer's version moves when a restart replays its registration over a seeded
            // registry, which follows the deployment's write history rather than any label.
            let mut layers = body["layers"].clone();
            for layer in layers.as_array_mut().unwrap() {
                layer.as_object_mut().unwrap().remove("version");
            }
            layers
        }
    };
    assert_eq!(
        meta(&a.server, blue_a.clone()).await,
        meta(&b.server, blue_b.clone()).await
    );
    compared += 1;
    compared
}

/// The control: a principal holding `red` is served what the other is not, in the same kind of
/// request, so the equalities above are about the label and not about an empty layer.
async fn assert_red_is_served(a: &Deployment) {
    let red = token(&a.server, &["0", "red"]).await;
    let blue = token(&a.server, &["0", "blue"]).await;
    let for_red = keys_served(&viewport_raw(&a.server, &red, viewport(0, json!({}))).await);
    let for_blue = keys_served(&viewport_raw(&a.server, &blue, viewport(0, json!({}))).await);
    for key in ["p-red", "c-red", "list-red", "n-red"] {
        assert!(for_red.contains(&key.to_string()), "{key} in {for_red:?}");
        assert!(!for_blue.contains(&key.to_string()), "{key} in {for_blue:?}");
    }
    assert!(!for_red.contains(&"shared".to_string()), "blue's own is withheld from red");
    assert!(for_blue.contains(&"shared".to_string()));
    for key in ["open", "p-open", "c-open", "n-open"] {
        assert!(for_red.contains(&key.to_string()) && for_blue.contains(&key.to_string()));
    }
    // A label list admits a viewer holding any one of its labels.
    let green = token(&a.server, &["0", "green"]).await;
    let for_green = keys_served(&viewport_raw(&a.server, &green, viewport(0, json!({}))).await);
    assert!(for_green.contains(&"list-red".to_string()));
    assert!(!for_green.contains(&"p-red".to_string()));
}

/// **A withheld artifact is indistinguishable from one never published**, on every viewer route,
/// and stays so across a restart and a fold.
#[tokio::test]
async fn an_artifact_withheld_by_its_own_label_is_indistinguishable_from_one_never_published() {
    let a = deployment(true).await;
    let b = deployment(false).await;
    assert_red_is_served(&a).await;
    assert!(assert_indistinguishable(&a, &b).await > 50);

    // Restart, both alike: the labels come back from the log.
    let (a, b) = (restart(a).await, restart(b).await);
    assert_red_is_served(&a).await;
    assert_indistinguishable(&a, &b).await;

    // A fold rewrites every level into extents; the labels are carried with the records.
    fold(&a.server).await;
    fold(&b.server).await;
    let (a, b) = (restart(a).await, restart(b).await);
    assert_red_is_served(&a).await;
    assert_indistinguishable(&a, &b).await;
}

async fn restart(d: Deployment) -> Deployment {
    let Deployment { tmp, server, ids } = d;
    server.shutdown().await;
    Deployment {
        server: open(&tmp).await,
        tmp,
        ids,
    }
}

async fn wait_until(
    server: &TestServer,
    what: &str,
    done: impl Fn(&tessera_engine::ExecutorStats) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if done(&server.state.engine.write_executor_stats()) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what}: never happened");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Ingest one point, flush, then fold: a flush with nothing buffered publishes nothing.
async fn fold(server: &TestServer) {
    let ingested = external_id_of(9_001);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "labels-fold")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(Some(&ingested[..]), 10.0, 10.0, "0")]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{}", resp.text().await.unwrap());
    tick(server).await;
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the fold published", move |now| {
        assert_eq!(now.fold_failures, before.fold_failures);
        now.folds > before.folds
    })
    .await;
}

/// A label is set once: filled on an artifact that has none, accepted again unchanged, and a
/// different one is a `409` that changes nothing. A label on a layer naming no field is a `422`.
#[tokio::test]
async fn a_label_fills_once_and_a_layer_naming_no_field_refuses_one() {
    let d = deployment(false).await;
    let server = &d.server;
    let blue = token(server, &["0", "blue"]).await;
    let served = |bytes: Vec<u8>| keys_served(&bytes);

    assert!(served(viewport_raw(server, &blue, viewport(0, json!({}))).await)
        .contains(&"open".to_string()));
    let (status, body) = patch(server, LAYER, json!([{ "key": "open", "access": ["red"] }])).await;
    assert_eq!(status, 200, "{body}");
    tick(server).await;
    assert!(!served(viewport_raw(server, &blue, viewport(0, json!({}))).await)
        .contains(&"open".to_string()));

    let (status, _) = patch(server, LAYER, json!([{ "key": "open", "access": ["red"] }])).await;
    assert_eq!(status, 200);
    let (status, _) = patch(server, LAYER, json!([{ "key": "open", "access": ["blue"] }])).await;
    assert_eq!(status, 409);
    tick(server).await;
    assert!(!served(viewport_raw(server, &blue, viewport(0, json!({}))).await)
        .contains(&"open".to_string()));

    let resp = server
        .client
        .put(artifacts_url(server, NAMES))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": [
            { "key": "n-x", "attached_to": { "layer": LAYER, "key": "p-open" },
              "content": [{ "values": ["X"] }], "access": ["red"] }
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
}

/// An artifact with no label takes the layer's default: a named default admits only a viewer
/// holding it.
#[tokio::test]
async fn an_unlabelled_artifact_takes_a_named_default() {
    let tmp = TempDir::new().unwrap();
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = open(&tmp).await;
    declare(&server, teams_declaration("red")).await;
    publish(
        &server,
        LAYER,
        json!([
            { "key": "bare", "members": members(0..40) },
            { "key": "blue", "members": members(0..40), "access": ["blue"] },
        ]),
    )
    .await;
    tick(&server).await;
    let red = token(&server, &["0", "red"]).await;
    let blue = token(&server, &["0", "blue"]).await;
    assert_eq!(
        keys_served(&viewport_raw(&server, &red, viewport(0, json!({}))).await),
        vec!["bare".to_string()]
    );
    assert_eq!(
        keys_served(&viewport_raw(&server, &blue, viewport(0, json!({}))).await),
        vec!["blue".to_string()]
    );
}

