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
            body["layers"].clone()
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


// ---------------------------------------------------------------------------------------------
// A build is an ingest into an empty database
// ---------------------------------------------------------------------------------------------

/// One artifact both sides hold: key, members, labels. `None` is no label.
type Built = (&'static str, std::ops::Range<u64>, Option<&'static [&'static str]>);

/// The artifacts both sides hold.
const BUILT: [Built; 4] = [
    ("open", 0..40, None),
    ("red", 40..80, Some(&["red"])),
    ("either", 80..120, Some(&["blue", "red"])),
    ("blue", 120..160, Some(&["blue"])),
];

/// Write the artifact source, its label column spelled as `labels` says: a list of strings, or a
/// plain string where each artifact carries at most one label.
fn write_teams(path: &std::path::Path, list: bool) {
    use arrow::array::{ArrayRef, ListBuilder, StringArray, StringBuilder, UInt64Builder};
    use arrow::datatypes::{DataType, Field, Schema};
    let keys = StringArray::from(BUILT.iter().map(|(k, _, _)| *k).collect::<Vec<_>>());
    let mut members = ListBuilder::new(UInt64Builder::new());
    for (_, range, _) in &BUILT {
        for e in range.clone() {
            members.values().append_value(e);
        }
        members.append(true);
    }
    let team: ArrayRef = if list {
        let mut team = ListBuilder::new(StringBuilder::new());
        for (_, _, labels) in &BUILT {
            match labels {
                None => team.append(false),
                Some(labels) => {
                    for label in *labels {
                        team.values().append_value(label);
                    }
                    team.append(true);
                }
            }
        }
        std::sync::Arc::new(team.finish())
    } else {
        std::sync::Arc::new(StringArray::from(
            BUILT
                .iter()
                .map(|(_, _, labels)| labels.map(|l| l[0]))
                .collect::<Vec<_>>(),
        ))
    };
    let members: ArrayRef = std::sync::Arc::new(members.finish());
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("members", members.data_type().clone(), false),
        Field::new("team", team.data_type().clone(), true),
    ]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema.clone(),
        vec![std::sync::Arc::new(keys), members, team],
    )
    .unwrap();
    let mut w =
        parquet::arrow::ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
            .unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn built_config(field: &str) -> String {
    format!(
        r#"
[sources]
points = "points.parquet"
teams  = "teams.parquet"

[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 1000.0 }}
point_visibility = {{ default = "public" }}

[[layer]]
name                      = "{LAYER}"
title                     = "{LAYER}"
views                     = ["s0"]
source                    = "teams"
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ field = "{field}", default = "inherited" }}
require_member_visibility = "none"
hierarchy                 = {{ kind = "nested", prune_children = false }}
content                   = {{ computed = ["centroid"] }}

[[layer]]
name                      = "inline"
title                     = "inline"
views                     = ["s0"]
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ field = "team", default = "inherited" }}
require_member_visibility = "none"
hierarchy                 = {{ kind = "flat", prune_children = false }}
artifacts = [
  {{ key = "inline-red", members = [0, 1, 2], access = "red" }},
  {{ key = "inline-open", members = [3, 4, 5] }},
]
"#
    )
}

/// Build the fixture's points with the two layers above; the build's own answer.
fn build_labelled(dir: &std::path::Path, list: bool, field: &str) -> Result<(), String> {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points_n(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    write_teams(&dir.join("teams.parquet"), list);
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, built_config(field)).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .map_err(|e| e.to_string())?;
    let args = tessera_build::BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &config.schema),
        out: dir.join("bundle"),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    tessera_build::build(&args).map(|_| ()).map_err(|e| e.to_string())
}

/// Keys and masked counts served to one principal, per layer.
async fn served_counts(server: &TestServer, terms: &[&str]) -> Vec<(String, String, u64)> {
    let token = token(server, terms).await;
    let mut rows: Vec<(String, String, u64)> =
        decode_viewport_frames(&viewport_raw(server, &token, viewport(0, json!({}))).await)
            .artifacts
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.layer, row.key.unwrap_or_default(), row.masked_count))
            .collect();
    rows.sort();
    rows
}

/// **The same labelled artifacts, built and published, serve alike** to every principal, whether
/// the build reads the label column as a list or as a plain string.
#[tokio::test]
async fn labels_built_and_labels_published_serve_alike() {
    for list in [true, false] {
        let built = TempDir::new().unwrap();
        build_labelled(built.path(), list, "team").expect("the labelled build succeeds");
        let built_server = open(&built).await;

        let live = TempDir::new().unwrap();
        build_fixture(
            &live.path().join("bundle"),
            &live.path().join("points.parquet"),
            &live.path().join("pairs.parquet"),
        );
        let live_server = open(&live).await;
        declare(&live_server, teams_declaration("inherited")).await;
        let mut inline = teams_declaration("inherited");
        inline["name"] = json!("inline");
        inline["title"] = json!("inline");
        inline["hierarchy"]["kind"] = json!("flat");
        inline["content"]["computed"] = json!([]);
        declare(&live_server, inline).await;
        publish(
            &live_server,
            LAYER,
            json!(BUILT
                .iter()
                .map(|(key, range, labels)| {
                    // A plain string column carries the first label alone.
                    let labels: Option<Vec<&str>> =
                        labels.map(|l| if list { l.to_vec() } else { vec![l[0]] });
                    json!({ "key": key, "members": members(range.clone()), "access": labels })
                })
                .collect::<Vec<_>>()),
        )
        .await;
        publish(
            &live_server,
            "inline",
            json!([
                { "key": "inline-red", "members": members(0..3), "access": ["red"] },
                { "key": "inline-open", "members": members(3..6) },
            ]),
        )
        .await;
        tick(&live_server).await;

        for terms in [&["0"][..], &["0", "red"], &["0", "blue"], &["1", "red", "blue"]] {
            let from_build = served_counts(&built_server, terms).await;
            assert_eq!(
                from_build,
                served_counts(&live_server, terms).await,
                "{terms:?}, list {list}"
            );
            assert!(!from_build.is_empty());
        }
        built_server.shutdown().await;
        live_server.shutdown().await;
    }
}

/// A declared label field its source does not carry is refused by the build.
#[test]
fn a_label_field_the_source_does_not_carry_is_refused() {
    let dir = TempDir::new().unwrap();
    assert!(build_labelled(dir.path(), true, "squad").is_err());
    let dir = TempDir::new().unwrap();
    assert!(build_labelled(dir.path(), true, "team").is_ok(), "the same build naming the column");
}

// ---------------------------------------------------------------------------------------------
// A label the plugin maps to nothing
// ---------------------------------------------------------------------------------------------

/// `builtin:passthrough` in every respect but one: the label `nothing` maps to no descriptor.
struct Forgetful;

impl tessera_plugin::Plugin for Forgetful {
    fn terms_of_labels(
        &self,
        labels: &[tessera_plugin::Descriptor],
    ) -> Result<Vec<tessera_plugin::Descriptor>, tessera_plugin::PluginError> {
        let kept: Vec<_> = labels.iter().filter(|l| l.as_slice() != b"nothing").cloned().collect();
        tessera_plugin::Passthrough::new().terms_of_labels(&kept)
    }
    fn terms_of_auth(
        &self,
        auth_data: &[u8],
    ) -> Result<Vec<tessera_plugin::Descriptor>, tessera_plugin::PluginError> {
        tessera_plugin::Passthrough::new().terms_of_auth(auth_data)
    }
    fn present_terms(
        &self,
        descriptors: &[tessera_plugin::Descriptor],
    ) -> Result<Vec<String>, tessera_plugin::PluginError> {
        tessera_plugin::Passthrough::new().present_terms(descriptors)
    }
    fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        tessera_plugin::Passthrough::new().declared_bounds()
    }
    fn data_plugin_hash(&self) -> String {
        tessera_plugin::Passthrough::new().data_plugin_hash()
    }
    fn auth_plugin_hash(&self) -> String {
        tessera_plugin::Passthrough::new().auth_plugin_hash()
    }
}

/// **A label the plugin maps to no term is refused**, at publication and at a fill, rather than
/// stored as no label: on an `inherited` layer that would serve the artifact to everyone the layer
/// admits.
#[tokio::test]
async fn a_label_the_plugin_maps_to_nothing_is_refused_rather_than_stored_as_none() {
    let tmp = TempDir::new().unwrap();
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let config = default_engine_config();
    let max_k = config.max_k;
    let engine = tessera_engine::Engine::open(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Forgetful,
        config,
    )
    .unwrap();
    let server = spawn_server_from_engine(engine, max_k, generous_test_gate()).await;
    declare(&server, teams_declaration("inherited")).await;

    let resp = server
        .client
        .put(artifacts_url(&server, LAYER))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": [
            { "key": "hidden", "members": members(0..40), "access": ["nothing"] }
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);

    publish(&server, LAYER, json!([{ "key": "bare", "members": members(0..40) }])).await;
    let (status, _) = patch(&server, LAYER, json!([{ "key": "bare", "access": ["nothing"] }])).await;
    assert_eq!(status, 422);
    tick(&server).await;
    assert_eq!(
        keys_served(&viewport_raw(&server, &token(&server, &["0"]).await, viewport(0, json!({}))).await),
        vec!["bare".to_string()],
        "nothing named `hidden` was stored"
    );
}
