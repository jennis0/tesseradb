//! **`POST /control/values`** (`ingest.md` §1.4; decision 0136, track T3): a batch of attribute
//! values over entities that already exist, addressed by `external_id` or `tessera_id`, in JSON
//! by default and Arrow by content type. What this file pins is the wire — the status codes and
//! bodies the route answers, the two encodings landing identical values, the view header a
//! group-scoped column needs, the two pagination units, and the identifier the route refuses.
//!
//! The engine-level cases — every family's fill, the claimant read, the values-only tick, the
//! fold and the replay — are `tessera-engine/tests/values_fill.rs`.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};

const N: u64 = 40;

/// One rendered, indexed `f32` at the build, and two `public` vocabularies no build column names,
/// one closed and one open, so a runtime category over either fixes the width at the declaration.
const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "dept"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  eng = 5
  ops = 6

[[vocabulary]]
name       = "grade"
width      = "u16"
value_set  = "open"
visibility = "public"

[[attribute]]
name   = "score"
type   = "f32"
render = true
index  = true
"#;

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Float32, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let scores: Vec<f32> = ids.iter().map(|e| (*e % 7) as f32).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(Float32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_fixture_bundle(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, N);
    write_pairs_n(&pairs, N);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .unwrap()
        .schema;
    let out = dir.join("bundle");
    build(&BuildArgs {
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
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("fixture build should succeed");
    out
}

struct Served {
    server: TestServer,
    /// Held so the tempdir outlives the server it serves from.
    #[allow(dead_code)]
    tmp: TempDir,
}

/// A served fixture with two runtime columns declared: an indexed keyword and an indexed
/// category, each of which a values batch can fill.
async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    build_fixture_bundle(tmp.path());
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let served = Served { server, tmp };
    declare(&served, json!({"name": "tag", "type": "keyword", "index": true})).await;
    declare(
        &served,
        json!({"name": "dept", "type": "category", "vocabulary": "dept", "index": true}),
    )
    .await;
    served
}

/// Stop the server and open the same bundle and log again.
async fn restart(served: Served) -> Served {
    let Served { server, tmp } = served;
    server.shutdown().await;
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    Served { server, tmp }
}

async fn declare(served: &Served, body: Value) {
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/attributes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(
        status == 200 || status == 201,
        "the declaration is accepted: {status} {answer}"
    );
}

/// Ingest one point and answer its `tessera_id`.
async fn ingest_point(served: &Served, batch_id: &str, external_id: &str) -> u64 {
    ingest_point_with(served, batch_id, external_id, json!({})).await
}

/// [`ingest_point`], with `columns` added to the row.
async fn ingest_point_with(
    served: &Served,
    batch_id: &str,
    external_id: &str,
    columns: Value,
) -> u64 {
    let mut row = json!({
        "external_id": base64_of(external_id),
        "x": 500.0,
        "y": 500.0,
        "access": ["0"],
        "score": 1.0,
    });
    for (name, value) in columns.as_object().unwrap() {
        row[name] = value.clone();
    }
    let body = json!([row]);
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the point is ingested: {answer}");
    answer["tessera_ids"][0]
        .as_u64()
        .or_else(|| answer["tessera_ids"][0].as_str().and_then(|s| s.parse().ok()))
        .unwrap()
}

fn base64_of(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

/// One `POST /control/values` request, with the view header where `view` says so.
async fn values(
    served: &Served,
    batch_id: &str,
    view: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut request = served
        .server
        .client
        .post(served.server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id);
    if let Some(view) = view {
        request = request.header("x-tessera-view", view);
    }
    let resp = request.json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The same batch as an Arrow IPC stream over the same two columns.
fn arrow_values(external_id: &str, tag: &str) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("tag", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::BinaryArray::from_iter(
                [Some(external_id.as_bytes())].into_iter(),
            )),
            Arc::new(StringArray::from_iter([Some(tag)])),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn flush(served: &Served) {
    let before = served.server.state.engine.write_executor_stats().flushes;
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.server.state.engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The drill-down's fields for one `tessera_id`, by name.
async fn item_fields(served: &Served, id: u64) -> Value {
    let token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{id}")))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    body["fields"].clone()
}

async fn status(served: &Served) -> Value {
    let resp = served
        .server
        .client
        .get(served.server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

// ---------------------------------------------------------------------------------------------

/// **The route fills, restates and refuses on the wire** (`ingest.md` §1.1, §1.4). A `200` names
/// what the batch did; a restatement fills nothing; a cell supplied with a different value is a
/// `409` naming the column and never the held value; a row naming an entity this deployment does
/// not hold is a `422`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_values_route_fills_restates_and_refuses() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "tag": "alpha", "dept": "ops"}]),
    )
    .await;
    assert_eq!(status, 200, "the fill is accepted: {answer}");
    assert_eq!(answer["rows"], 1);
    assert_eq!(answer["filled"], 2, "one cell per column: {answer}");
    assert_eq!(answer["held"], 0);
    flush(&served).await;

    let fields = item_fields(&served, id).await;
    assert_eq!(fields["tag"], json!("alpha"), "{fields}");
    assert_eq!(fields["dept"], json!("ops"), "{fields}");

    // A restatement under a fresh batch id fills nothing and is counted as held.
    let (status, answer) = values(
        &served,
        "values-2",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["filled"], 0);
    assert_eq!(answer["held"], 1);

    // A different value is a 409 naming the column, and never either value.
    let (status, answer) = values(
        &served,
        "values-3",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "tag": "beta"}]),
    )
    .await;
    assert_eq!(status, 409, "{answer}");
    let detail = answer.to_string();
    assert!(detail.contains("column 'tag'"), "{detail}");
    assert!(
        !detail.contains("alpha") && !detail.contains("beta"),
        "the body names neither value (`ingest.md` §1.4): {detail}"
    );

    // A row naming an entity this deployment does not hold is a 422: a values batch creates
    // nothing, so the remedy is to ingest the point first.
    let (status, answer) = values(
        &served,
        "values-4",
        Some("s0"),
        json!([{"external_id": base64_of("nobody"), "tag": "gamma"}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer.to_string().contains("row 0"),
        "the refusal names the row and not the id: {answer}"
    );
}

/// The keys a viewer route that lists a column's values answers, on one page.
async fn listed_keys(served: &Served, path: &str) -> Vec<String> {
    let token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .get(served.server.viewer_url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap().to_string())
        .collect()
}

/// `grade`'s value list and its suggestions both carry `key`.
async fn assert_minted_key_is_listed(served: &Served, key: &str) {
    for path in ["/v1/categories/grade", "/v1/categories/grade/suggest?q=g"] {
        let keys = listed_keys(served, path).await;
        assert!(keys.iter().any(|k| k == key), "{path} lists {keys:?}");
    }
}

/// A key of an open vocabulary that no ingest has used is minted by the values batch that names
/// it, and the cell is served after a flush and after a restart. A closed vocabulary's unknown key
/// is refused with nothing written, and the next flush still publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_values_batch_mints_a_new_key_of_an_open_vocabulary() {
    let served = serve().await;
    declare(
        &served,
        json!({"name": "grade", "type": "category", "vocabulary": "grade", "index": true}),
    )
    .await;
    let id = ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "grade": "g0"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["filled"], 1, "{answer}");

    let (status, answer) = values(
        &served,
        "values-2",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "dept": "legal"}]),
    )
    .await;
    assert_eq!(status, 422, "a closed vocabulary's unknown key is refused: {answer}");

    flush(&served).await;
    let fields = item_fields(&served, id).await;
    assert_eq!(fields["grade"], json!("g0"), "{fields}");
    assert!(fields["dept"].is_null(), "the refused batch wrote nothing: {fields}");
    assert_minted_key_is_listed(&served, "g0").await;

    let served = restart(served).await;
    assert_eq!(item_fields(&served, id).await["grade"], json!("g0"));
    assert_minted_key_is_listed(&served, "g0").await;
    let second = ingest_point(&served, "points-2", "second").await;
    flush(&served).await;
    assert!(
        item_fields(&served, second).await["grade"].is_null(),
        "a flush after the restart publishes"
    );
}

/// The keys of the artifacts a viewport over the whole frame serves from `layer`.
async fn served_keys(served: &Served, layer: &str) -> Vec<String> {
    let token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "layers": "all"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut keys: Vec<String> = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.layer == layer)
        .filter_map(|a| a.key)
        .collect();
    keys.sort();
    keys
}

/// A layer whose artifacts are the values of a column gets an artifact for a new value whether
/// the value arrives by ingest or by a values batch, and both survive a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_value_filled_by_a_values_batch_derives_its_artifact_as_ingest_does() {
    let served = serve().await;
    declare(
        &served,
        json!({"name": "grade", "type": "category", "vocabulary": "grade", "index": true}),
    )
    .await;
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": "grades",
            "title": "Grades",
            "views": ["s0"],
            "membership": { "attribute": "grade" },
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": [] },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap_or_default());

    ingest_point_with(&served, "points-1", "by-ingest", json!({"grade": "g1"})).await;
    ingest_point(&served, "points-2", "by-values").await;
    flush(&served).await;
    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"external_id": base64_of("by-values"), "grade": "g2"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    flush(&served).await;
    assert_eq!(served_keys(&served, "grades").await, ["g1", "g2"]);

    let served = restart(served).await;
    assert_eq!(served_keys(&served, "grades").await, ["g1", "g2"]);
}

/// **The same batch as JSON and as Arrow lands identical values** (`ingest.md` §1.2). Nothing
/// about the route's semantics depends on which encoding carried it: the Arrow batch fills the
/// cell, and the JSON restatement of it is the fill rule's no-op rather than a second claimant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_encodings_land_identical_values() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "values-arrow")
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(arrow_values("subject", "alpha"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the Arrow batch is accepted: {answer}");
    assert_eq!(answer["filled"], 1);
    flush(&served).await;
    assert_eq!(item_fields(&served, id).await["tag"], json!("alpha"));

    // The JSON spelling of the same batch: the cell is held identically, so it is a no-op.
    let (status, answer) = values(
        &served,
        "values-json",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["filled"], 0, "{answer}");
    assert_eq!(answer["held"], 1, "{answer}");
}

/// **A row may name its entity by `tessera_id` and its identifier set** (`ingest.md` §1.4), on
/// `/control/changes`' rule, and one without the set is refused: an identifier's meaning depends
/// on the key it was minted under.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_addressed_by_tessera_id_carries_its_idset() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"tessera_id": id.to_string(), "idset": FIXTURE_IDSET, "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["filled"], 1);

    let (status, answer) = values(
        &served,
        "values-2",
        Some("s0"),
        json!([{"tessera_id": id.to_string(), "tag": "beta"}]),
    )
    .await;
    assert_eq!(status, 422, "a tessera_id with no idset is refused: {answer}");
    assert!(answer.to_string().contains("idset"), "{answer}");
}

/// **A group-scoped column is nameable only on a batch that carries the view header**
/// (`ingest.md` §1.4, `views.md` §5). This bundle declares no group, so `sentiment` is a name
/// nothing declares and takes the undeclared-column refusal — which is the same refusal a scoped
/// column takes on a viewless batch, the families a batch may name being empty without a header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_column_is_refused_and_names_the_view() {
    let served = serve().await;
    ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"external_id": base64_of("subject"), "sentiment": 0.5}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
    let detail = answer.to_string();
    assert!(detail.contains("'sentiment'"), "{detail}");
    assert!(
        detail.contains("group-scoped family whose key set holds this batch's view"),
        "the refusal names the scoped route a column could have been declared through: {detail}"
    );

    // And a batch that names no view at all takes the same refusal, which is what keeps a scoped
    // column un-nameable without a header.
    let (status, answer) = values(
        &served,
        "values-2",
        None,
        json!([{"external_id": base64_of("subject"), "sentiment": 0.5}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
}

/// **The route publishes its two pagination units and enforces them** (`ingest.md` §2.1): a
/// record count and a byte cap on `/control/status`, each a `422` naming the unit rather than a
/// truncation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limits_are_published_and_enforced() {
    let served = serve().await;
    let limits = status(&served).await["limits"]["values"].clone();
    assert_eq!(limits["route"], json!("POST /control/values"));
    let max_rows = limits["max_batch_rows"].as_u64().expect("a record count");
    assert!(
        limits["max_batch_bytes"].as_u64().is_some(),
        "and a byte cap: {limits}"
    );

    ingest_point(&served, "points-1", "subject").await;
    flush(&served).await;

    // One row over the published count, refused naming the unit before anything is appended.
    let rows: Vec<Value> = (0..=max_rows)
        .map(|i| json!({"external_id": base64_of(&format!("row-{i}")), "tag": "alpha"}))
        .collect();
    let (status, answer) = values(&served, "values-1", Some("s0"), Value::Array(rows)).await;
    assert_eq!(status, 422, "{answer}");
    assert!(
        answer.to_string().contains("ingest_max_batch_rows"),
        "the refusal names the unit: {answer}"
    );
}
