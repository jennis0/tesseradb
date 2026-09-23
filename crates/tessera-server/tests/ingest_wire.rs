//! **JSON on every record-bearing route, Arrow by content type, and one `limits` block** (ingest
//! §1.2, §2.1; decision 0136 rulings 1 and 2).
//!
//! What is asserted, over the real routes and through served answers rather than bytes: a batch
//! sent as JSON and the same batch sent as Arrow land rows a viewer cannot tell apart; a cell that
//! does not coerce to its column is a 422 naming the row and the column, whole batch without
//! effect; a row with no label at the JSON door takes the view's declared default or is refused
//! with the count (decision 0133), as at the Arrow door; every limit `/control/status` publishes
//! is the one the route refuses over, at exactly that value; a growth page as Arrow lands what
//! the same page as JSON lands; and a content type naming neither encoding is refused.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, BinaryArray, Float32Array, Float64Array, Int64Array, StringArray,
    TimestampMicrosecondArray, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

const ARROW: &str = "application/vnd.apache.arrow.stream";
const LAYER: &str = "clusters/wire";

/// The declared tail: one column per family a JSON cell must coerce into. `score` and `tag` are
/// indexed so a filter can tell the two encodings' rows apart, or fail to; `big` is a `u64` past
/// 2⁵³, which a decode through a double would move.
const SCHEMA: &str = r#"
[[attribute]]
name  = "score"
type  = "i64"
index = true

[[attribute]]
name   = "weight"
type   = "f32"
render = true

[[attribute]]
name = "big"
type = "u64"

[[attribute]]
name = "seen"
type = "timestamp_us"

[[attribute]]
name  = "tag"
type  = "keyword"
index = true
"#;

const N: u64 = 20;

fn write_points(path: &Path) {
    let score = Int64Array::from_iter_values((0..N).map(|e| -(e as i64)));
    write_points_scored(path, Arc::new(score));
}

/// The fixture's points file with `score`, declared `i64`, carried by `score` as written.
fn write_points_scored(path: &Path, score: arrow::array::ArrayRef) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", score.data_type().clone(), true),
        Field::new("weight", DataType::Float32, true),
        Field::new("big", DataType::UInt64, true),
        Field::new(
            "seen",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("tag", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|e| ((e * 37) % 1000) as f64),
            )),
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|e| ((e * 53) % 1000) as f64),
            )),
            score,
            Arc::new(Float32Array::from_iter_values(
                ids.iter().map(|e| *e as f32 / 4.0),
            )),
            Arc::new(UInt64Array::from_iter_values(ids.iter().map(|e| e * 1_000))),
            Arc::new(TimestampMicrosecondArray::from_iter_values(
                ids.iter().map(|e| *e as i64 * 1_000_000),
            )),
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|e| format!("built-{e}")),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_bundle(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    write_points(&points);
    build_over(dir, points).expect("the build succeeds")
}

/// Build the fixture's declaration over `points`.
fn build_over(
    dir: &Path,
    points: std::path::PathBuf,
) -> Result<std::path::PathBuf, tessera_build::BuildError> {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, N);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA).unwrap();
    let config = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the declaration parses");
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
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.clone(),
            &config.schema,
        ),
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
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    })?;
    Ok(out)
}

/// A server over the declared-tail bundle, at the harness's generous limits.
async fn served_declared() -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let bundle = build_bundle(tmp.path());
    let engine = engine_over(&bundle, tmp.path());
    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        generous_ingest_limits(),
    )
    .await;
    (tmp, server)
}

fn engine_over(bundle: &Path, dir: &Path) -> Engine {
    let mut engine = Engine::open(
        bundle,
        &dir.join("cache"),
        &dir.join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            // Not the harness default: a flush is pulled by hand below and must publish every
            // buffered row, so the tick's own row trigger is left out of the way.
            ..default_engine_config()
        },
    )
    .unwrap();
    engine.start_write_executor(1024).unwrap();
    engine
}

/// One row of the declared tail, as the two encodings spell it.
#[derive(Clone)]
struct Row {
    id: u64,
    x: f32,
    y: f32,
    score: i64,
    weight: f32,
    big: u64,
    seen: i64,
    tag: &'static str,
    cluster: &'static str,
}

/// Every value is a function of `id % 10`, so a row of one ten-id range has a twin in every
/// other: the same values under another external id.
fn rows(ids: std::ops::Range<u64>) -> Vec<Row> {
    ids.map(|id| {
        let r = id % 10;
        Row {
            id,
            x: 10.0 + r as f32,
            y: 20.0 + 2.0 * r as f32,
            score: 1_000 + (r % 3) as i64,
            weight: 0.25 * (r % 4) as f32,
            big: 9_223_372_036_854_775_809 + (r % 2),
            seen: 1_700_000_000_000_000 + r as i64,
            tag: if r % 2 == 0 { "even" } else { "odd" },
            cluster: if r % 3 == 0 { "k0" } else { "k1" },
        }
    })
    .collect()
}

fn arrow_body(rows: &[Row]) -> Vec<u8> {
    let access = access_column(rows.iter().map(|_| "0"));
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::Int64, true),
        Field::new("weight", DataType::Float32, true),
        Field::new("big", DataType::UInt64, true),
        Field::new(
            "seen",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("tag", DataType::Utf8, true),
        Field::new(LAYER, DataType::Utf8, true),
    ]));
    let ids: Vec<Vec<u8>> = rows.iter().map(|r| external_id_of(r.id)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                ids.iter().map(|v| v.as_slice()),
            )),
            Arc::new(Float32Array::from_iter_values(rows.iter().map(|r| r.x))),
            Arc::new(Float32Array::from_iter_values(rows.iter().map(|r| r.y))),
            Arc::new(access),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.score))),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|r| r.weight),
            )),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.big))),
            Arc::new(TimestampMicrosecondArray::from_iter_values(
                rows.iter().map(|r| r.seen),
            )),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.tag))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.cluster),
            )),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// One JSON record. `big` travels as a string of digits, which is what a JavaScript client would
/// have to send; `seen` as the integer microseconds the roster's `timestamp_us` takes.
fn json_record(row: &Row) -> Value {
    json!({
        "external_id": b64(&external_id_of(row.id)),
        "x": row.x,
        "y": row.y,
        "access": ["0"],
        "score": row.score,
        "weight": row.weight,
        "big": row.big.to_string(),
        "seen": row.seen,
        "tag": row.tag,
        LAYER: row.cluster,
    })
}

fn json_array_body(rows: &[Row]) -> Vec<u8> {
    Value::Array(rows.iter().map(json_record).collect())
        .to_string()
        .into_bytes()
}

fn ndjson_body(rows: &[Row]) -> Vec<u8> {
    rows.iter()
        .map(|row| json_record(row).to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

async fn ingest(
    server: &TestServer,
    batch_id: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (u16, Value) {
    let mut request = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let resp = request.body(body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn register_layer(server: &TestServer, value_set: &str) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": LAYER,
            "title": LAYER,
            "views": ["s0"],
            "membership": "enumerated",
            "value_set": value_set,
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
    assert_eq!(resp.status().as_u16(), 201, "the layer registers");
}

fn artifacts_url(server: &TestServer) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        LAYER.replace('/', "%2F")
    ))
}

/// Pull one tick and wait until the buffer is empty and the flush has published.
async fn flush(server: &TestServer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let before = server.state.engine.write_executor_stats().flushes;
        let resp = server
            .client
            .post(server.control_url("/control/flush"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        while server.state.engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if server.state.engine.buffered_items() == 0 {
            break;
        }
    }
}

/// The full viewport for `terms`, with an optional filter and the layer selected: its point
/// identifiers and the layer's artifact rows, waited past any refresh.
async fn viewport(
    server: &TestServer,
    terms: &[&str],
    filter: Option<Value>,
) -> (Vec<u64>, Vec<ArtifactRow>) {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let mut request = json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "layers": [LAYER],
        });
        if let Some(filter) = &filter {
            request["filters"] = filter.clone();
        }
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&request)
            .send()
            .await
            .unwrap();
        let unsettled = std::time::Instant::now() < deadline;
        if resp.status().as_u16() == 429 && unsettled {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(resp.status().as_u16(), 200, "the viewport answers");
        if resp
            .headers()
            .get("x-tessera-stale")
            .is_some_and(|v| v == "1")
            && unsettled
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        let frames = decode_viewport_frames(&resp.bytes().await.unwrap());
        let ids = frames.points.iter().map(|(id, _)| *id).collect();
        return (ids, frames.artifacts.unwrap_or_default());
    }
}

/// The drill-down of one item, with the two identifiers that differ by construction removed,
/// at every depth, so two rows carrying the same values compare equal.
async fn record_without_ids(server: &TestServer, token: &str, tessera_id: u64) -> Value {
    let resp = post_item(server, token, tessera_id).await;
    assert_eq!(resp.status().as_u16(), 200);
    let mut body: Value = resp.json().await.unwrap();
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("tessera_id");
                map.remove("external_id");
                map.remove("id");
                for v in map.values_mut() {
                    strip(v);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut body);
    body
}

/// **The headline.** Ten rows as Arrow, the same ten values as a JSON array under other ids and
/// again as newline-delimited objects, and a viewer is served thirty rows it cannot tell apart:
/// every indexed filter matches three for one, the layer's artifact counts all three sets, and a
/// drill-down of one row from each encoding reads the same values, the `u64` past 2⁵³ included.
#[tokio::test]
async fn the_same_batch_as_json_and_as_arrow_lands_identical_rows() {
    let (_tmp, server) = served_declared().await;
    register_layer(&server, "open").await;
    let (status, body) = ingest(&server, "arrow", Some(ARROW), arrow_body(&rows(100..110))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 10);
    let (status, body) = ingest(
        &server,
        "json-array",
        Some("application/json"),
        json_array_body(&rows(200..210)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 10);
    // No content type at all is JSON, the default (ingest §1.2).
    let (status, body) = ingest(&server, "ndjson", None, ndjson_body(&rows(300..310))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 10);
    flush(&server).await;

    let (ids, artifacts) = viewport(&server, &["0"], None).await;
    assert_eq!(
        ids.len(),
        N as usize + 30,
        "every row of the three batches is served"
    );
    let mut by_key: Vec<(String, u64)> = artifacts
        .iter()
        .map(|a| (a.key.clone().unwrap_or_default(), a.masked_count))
        .collect();
    by_key.sort();
    // Residues 0, 3, 6 and 9 of each ten are `k0`: twelve members; the other eighteen are `k1`.
    assert_eq!(
        by_key,
        vec![("k0".to_string(), 12), ("k1".to_string(), 18)],
        "the layer column landed from both encodings"
    );

    for (filter, expected) in [
        (json!({ "score": { "eq": 1000 } }), 12),
        (json!({ "score": { "eq": 1001 } }), 9),
        (json!({ "score": { "eq": 1002 } }), 9),
        (json!({ "tag": { "eq": "even" } }), 15),
        (json!({ "tag": { "eq": "odd" } }), 15),
    ] {
        let (matched, _) = viewport(&server, &["0"], Some(filter.clone())).await;
        assert_eq!(matched.len(), expected, "{filter}");
    }

    // The rows scoring 1001 are residues 1, 4 and 7 of each range: three value shapes, and each
    // is served three times, once per encoding, reading back the same record whichever carried
    // it.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let (all, _) = viewport(&server, &["0"], Some(json!({ "score": { "eq": 1001 } }))).await;
    let records: Vec<Value> = {
        let mut out = Vec::new();
        for id in all {
            out.push(record_without_ids(&server, token, id).await);
        }
        out
    };
    assert_eq!(records.len(), 9);
    let text = records[0].to_string();
    assert!(
        text.contains("9223372036854775809") || text.contains("9223372036854775810"),
        "the u64 past 2⁵³ survived the door: {text}"
    );
    let distinct: std::collections::BTreeSet<String> =
        records.iter().map(|r| r.to_string()).collect();
    assert_eq!(
        distinct.len(),
        3,
        "one record shape per residue, whichever encoding carried it: {distinct:?}"
    );
    for shape in &distinct {
        assert_eq!(
            records.iter().filter(|r| r.to_string() == *shape).count(),
            3,
            "one from each encoding: {shape}"
        );
    }
}

/// A value that does not coerce to its column is a `422` naming the row and the column, and the
/// batch has no effect (ingest §1.2).
#[tokio::test]
async fn a_cell_that_does_not_coerce_is_refused_naming_row_and_column() {
    let (_tmp, server) = served_declared().await;
    register_layer(&server, "open").await;
    let high_water = control_status(&server).await["entity_id_high_water"].clone();
    let good = rows(400..403);
    let cases: Vec<(&str, Value, &str)> = vec![
        ("score", json!(1.5), "row 1, column 'score'"),
        ("score", json!("ten"), "row 1, column 'score'"),
        ("weight", json!("heavy"), "row 1, column 'weight'"),
        ("weight", json!(1.0e39), "row 1, column 'weight'"),
        ("big", json!(-1), "row 1, column 'big'"),
        ("seen", json!(1.0e6), "row 1, column 'seen'"),
        ("tag", json!(7), "row 1, column 'tag'"),
        ("x", json!("east"), "row 1, column 'x'"),
        (
            "external_id",
            json!("not base64!"),
            "row 1, column 'external_id'",
        ),
        ("access", json!("0"), "row 1, column 'access'"),
        ("access", json!([null]), "row 1, column 'access'"),
        (LAYER, json!(true), "row 1, column 'clusters/wire'"),
        ("unknown", json!(1), "row 1, column 'unknown'"),
    ];
    for (column, value, expected) in cases {
        let mut records: Vec<Value> = good.iter().map(json_record).collect();
        records[1][column] = value.clone();
        let (status, body) = ingest(
            &server,
            &format!("bad-{column}-{value}"),
            Some("application/json"),
            Value::Array(records).to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, 422, "{column} = {value}: {body}");
        assert_eq!(body["error"], "contract");
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains(expected), "{column} = {value}: {detail}");
    }
    // A declared column missing from a row, and a layer column that changes shape mid-column.
    let mut records: Vec<Value> = good.iter().map(json_record).collect();
    records[2].as_object_mut().unwrap().remove("score");
    let (status, body) = ingest(
        &server,
        "missing",
        Some("application/json"),
        Value::Array(records).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("row 2, column 'score'"),
        "{body}"
    );
    let mut records: Vec<Value> = good.iter().map(json_record).collect();
    records[0][LAYER] = json!(["k0"]);
    let (status, body) = ingest(
        &server,
        "shape",
        Some("application/json"),
        Value::Array(records).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("row 1, column 'clusters/wire'"),
        "{body}"
    );

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water,
        "no refused batch allocated an entity"
    );
    assert_eq!(
        server.state.engine.published_artifacts(),
        0,
        "nothing minted"
    );
}

/// **Decision 0133 at the JSON door, four cases.** An empty list, a null and an absent `access`
/// are each a row with no label: under a declared default all three land under it; under no
/// default the batch is refused naming the count of such rows and the view. An empty element is
/// no label at all and is refused. The Arrow door's four are in `access_list.rs`.
#[tokio::test]
async fn an_unlabelled_json_row_takes_the_declared_default_or_is_refused_with_the_count() {
    async fn served_with_default(default: Option<&str>) -> (TempDir, TestServer) {
        let tmp = TempDir::new().unwrap();
        let bundle_root = tmp.path().join("bundle");
        let pairs = tmp.path().join("pairs.parquet");
        build_fixture_with_access(
            &bundle_root,
            &tmp.path().join("points.parquet"),
            &pairs,
            N_ITEMS,
            tessera_build::config::AccessInput {
                source: tessera_build::config::AccessSource::Relation(pairs.clone()),
                default: default.map(str::to_string),
            },
        );
        let server = spawn_server(
            &bundle_root,
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
        )
        .await;
        (tmp, server)
    }
    fn body() -> Vec<u8> {
        let id = |i: u64| b64(&external_id_of(N_ITEMS + 500 + i));
        json!([
            { "external_id": id(0), "x": 10.0, "y": 10.0, "access": [] },
            { "external_id": id(1), "x": 11.0, "y": 11.0, "access": null },
            { "external_id": id(2), "x": 12.0, "y": 12.0 },
            { "external_id": id(3), "x": 13.0, "y": 13.0, "access": ["1"] },
        ])
        .to_string()
        .into_bytes()
    }
    async fn visible_to(server: &TestServer, terms: &[&str]) -> u64 {
        let auth = authorise(server, terms).await;
        let token = auth["token"].as_str().unwrap();
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0] }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
        tiles.iter().map(|t| t.1).sum()
    }

    let (_tmp, server) = served_with_default(Some("ir:sealed")).await;
    let before = visible_to(&server, &["ir:sealed"]).await;
    let (status, resp) = ingest(&server, "filled", Some("application/json"), body()).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["accepted"], 4);
    flush(&server).await;
    assert_eq!(
        visible_to(&server, &["ir:sealed"]).await,
        before + 3,
        "the three unlabelled rows landed under the declared default; the labelled row kept its own"
    );

    let (_tmp, server) = served_with_default(None).await;
    let high_water = control_status(&server).await["entity_id_high_water"].clone();
    let (status, resp) = ingest(&server, "refused", Some("application/json"), body()).await;
    assert_eq!(status, 422, "{resp}");
    let detail = resp["detail"].as_str().unwrap();
    assert!(
        detail.contains("3 row(s)") && detail.contains("declares no `point_visibility.default`"),
        "the refusal names the count and the rule: {detail}"
    );
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water,
        "the batch had no effect"
    );
    let (status, resp) = ingest(
        &server,
        "empty-element",
        Some("application/json"),
        json!([{ "external_id": b64(&external_id_of(N_ITEMS + 600)), "x": 1.0, "y": 1.0, "access": [""] }])
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "an empty element is no label at all: {resp}");
}

fn member(source_id: u64) -> String {
    b64(&external_id_of(source_id))
}

/// Rows at the plain fixture's shape, which declares no scalar tail.
fn plain_json_body(ids: std::ops::Range<u64>) -> Vec<u8> {
    Value::Array(
        ids.map(|id| {
            json!({
                "external_id": b64(&external_id_of(N_ITEMS + 1_000 + id)),
                "x": 10.0 + (id % 7) as f64,
                "y": 20.0 + (id % 5) as f64,
                "access": ["0"],
            })
        })
        .collect(),
    )
    .to_string()
    .into_bytes()
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

fn publish_body(artifacts: &[(&str, Vec<String>)]) -> Vec<u8> {
    json!({
        "addressing": "external",
        "artifacts": artifacts
            .iter()
            .map(|(key, members)| json!({ "key": key, "members": members }))
            .collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes()
}

async fn put_raw(server: &TestServer, content_type: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .put(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn patch_raw(server: &TestServer, content_type: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .patch(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn grow_json(artifacts: &[(&str, Vec<String>)]) -> Vec<u8> {
    publish_body(artifacts)
}

/// The growth route's Arrow form: `key` and `members` per row, the envelope in the schema's
/// metadata (ingest §1.2).
fn grow_arrow(artifacts: &[(&str, Vec<String>)]) -> Vec<u8> {
    use arrow::array::{ListBuilder, StringBuilder};
    let mut lists = ListBuilder::new(StringBuilder::new());
    for (_, members) in artifacts {
        for member in members {
            lists.values().append_value(member);
        }
        lists.append(true);
    }
    let lists = lists.finish();
    let schema = Arc::new(
        Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("members", lists.data_type().clone(), true),
        ])
        .with_metadata(
            [("addressing".to_string(), "external".to_string())]
                .into_iter()
                .collect(),
        ),
    );
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(
                artifacts.iter().map(|(key, _)| *key),
            )),
            Arc::new(lists),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// A server over the plain fixture at the limits given, with the layer registered.
async fn served_plain(limits: IngestLimits) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = engine_over(&bundle_root, tmp.path());
    let server = mount_server_with_ingest_limits(engine, 200, generous_test_gate(), limits).await;
    register_layer(&server, "closed").await;
    (tmp, server)
}

/// Every limit the block publishes is the one the route refuses over, at exactly that value,
/// and the refusal names the unit (ingest §2.1). The two byte caps are set from real bodies so
/// the at-cap leg and the over-cap leg differ by one byte.
#[tokio::test]
async fn every_limit_in_the_block_is_enforced_at_its_published_value() {
    let three_rows = plain_json_body(0..3);
    // Two artifacts of three members: the at-cap body, and long enough that every count leg
    // below (three artifacts of one member, a growth of four) sits under the byte cap.
    let two_artifacts = publish_body(&[("p1", members(0..3)), ("p2", members(3..6))]);
    let limits = IngestLimits {
        admission: 8,
        max_batch_rows: 3,
        max_batch_bytes: three_rows.len(),
        buffer_max_items: 10_000_000,
        publish_max_body_bytes: two_artifacts.len(),
        max_artifacts_per_request: 2,
        max_members_per_request: 3,
        // The exclusion bound has its own file (`artifact_exclusion.rs`), the list being one
        // artifact's rather than the request's.
        max_excluded_per_request: 1_000_000,
    };
    let (_tmp, server) = served_plain(limits).await;

    let status = control_status(&server).await;
    let block = &status["limits"];
    assert_eq!(block["ingest"]["max_batch_rows"], 3);
    assert_eq!(block["ingest"]["max_batch_bytes"], three_rows.len());
    assert_eq!(block["publish"]["max_artifacts_per_request"], 2);
    assert_eq!(block["publish"]["max_body_bytes"], two_artifacts.len());
    assert_eq!(block["publish"]["max_shape_vertices"], 1_000_000);
    assert_eq!(block["publish"]["max_excluded_per_request"], 1_000_000);
    assert_eq!(block["grow"]["max_members_per_request"], 3);
    assert_eq!(block["grow"]["max_body_bytes"], two_artifacts.len());
    assert_eq!(block["changes"]["max_changes_per_request"], 10_000);
    assert_eq!(block["changes"]["max_body_bytes"], 2 * 1024 * 1024);
    assert_eq!(block["declarations"]["max_records_per_request"], 1);
    assert_eq!(block["declarations"]["max_body_bytes"], 2 * 1024 * 1024);
    assert!(
        status["ingest"].get("max_batch_rows").is_none(),
        "the caps live in the block and nowhere else: {status}"
    );

    // Ingest: three rows at the byte cap land; a fourth row is over the byte cap; three wider
    // rows under a raised byte cap would be the row cap's, so the row cap is exercised on a
    // body one row longer with the bytes cap lifted below.
    let (status, body) = ingest(
        &server,
        "three",
        Some("application/json"),
        three_rows.clone(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let mut padded = three_rows.clone();
    padded.push(b' ');
    let (status, body) = ingest(&server, "padded", Some("application/json"), padded).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("ingest.ingest_max_batch_bytes"),
        "{body}"
    );

    // Publish: two artifacts at the byte cap land; the same body one byte longer is over it,
    // naming the key; three artifacts under the cap are over the count.
    let (status, body) = put_raw(&server, "application/json", two_artifacts.clone()).await;
    assert_eq!(status, 201, "{body}");
    let mut padded = two_artifacts.clone();
    padded.push(b' ');
    let (status, body) = put_raw(&server, "application/json", padded).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("ingest.publish_max_body_bytes"),
        "{body}"
    );
    let three = publish_body(&[
        ("a", members(6..7)),
        ("b", members(7..8)),
        ("c", members(8..9)),
    ]);
    assert!(
        three.len() <= two_artifacts.len(),
        "under the byte cap, so the count is the refusal"
    );
    let (status, body) = put_raw(&server, "application/json", three).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("ingest.max_artifacts_per_request"),
        "{body}"
    );
    assert_eq!(
        server.state.engine.published_artifacts(),
        2,
        "the refused publications allocated nothing"
    );

    // Grow: three members across the page land; four are over the count, under the byte cap.
    let (status, body) = patch_raw(
        &server,
        "application/json",
        grow_json(&[("p1", members(6..8)), ("p2", members(8..9))]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let four = grow_json(&[("p1", members(9..11)), ("p2", members(11..13))]);
    assert!(
        four.len() <= two_artifacts.len(),
        "under the byte cap, so the count is the refusal"
    );
    let (status, body) = patch_raw(&server, "application/json", four).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("ingest.max_members_per_request"),
        "{body}"
    );

    // Changes: ten thousand items are not refused for their count; one more is, before any
    // address is resolved.
    let items = |n: usize| -> Value {
        Value::Array(
            (0..n)
                .map(|i| json!({ "external_id": member(i as u64), "op": "suppress" }))
                .collect(),
        )
    };
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&items(10_001))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("max_changes_per_request"),
        "{body}"
    );
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&items(10_000))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(
        !body.to_string().contains("max_changes_per_request"),
        "ten thousand is at the limit, not over it: {status} {body}"
    );

    // The row cap, on its own server: the byte cap is generous, so a fourth row is the count's.
    let (_tmp, server) = served_plain(IngestLimits {
        max_batch_rows: 3,
        ..generous_ingest_limits()
    })
    .await;
    let (status, body) = ingest(
        &server,
        "three",
        Some("application/json"),
        plain_json_body(0..3),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ingest(
        &server,
        "four",
        Some("application/json"),
        plain_json_body(3..7),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("ingest.ingest_max_batch_rows"),
        "{body}"
    );
}

/// A growth page as Arrow lands what the same page as JSON lands: two artifacts published alike,
/// one grown by each encoding, serve the same masked count to each principal.
#[tokio::test]
async fn a_growth_page_as_arrow_lands_what_the_json_page_lands() {
    let (_tmp, server) = served_plain(generous_ingest_limits()).await;
    let (status, body) = put_raw(
        &server,
        "application/json",
        publish_body(&[("j", members(0..5)), ("a", members(0..5))]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let (status, body) = patch_raw(
        &server,
        "application/json",
        grow_json(&[("j", members(5..40))]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 35, "{body}");
    let (status, body) = patch_raw(&server, ARROW, grow_arrow(&[("a", members(5..40))])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 35, "{body}");
    assert_eq!(body["artifacts"][0]["key"], "a");

    for terms in [&["0"][..], &["1"][..]] {
        let (_, artifacts) = viewport(&server, terms, None).await;
        let count = |key: &str| {
            artifacts
                .iter()
                .find(|a| a.key.as_deref() == Some(key))
                .map(|a| a.masked_count)
                .unwrap_or(0)
        };
        assert_eq!(count("j"), count("a"), "{terms:?}: {artifacts:?}");
        assert!(count("j") > 5, "the growth landed: {artifacts:?}");
    }

    // The Arrow form carries what the JSON form carries and nothing else: a third column, an
    // unknown metadata key and a null `members` cell are each refused naming it.
    fn stream(
        fields: Vec<Field>,
        columns: Vec<Arc<dyn Array>>,
        metadata: &[(&str, &str)],
    ) -> Vec<u8> {
        let schema = Arc::new(
            Schema::new(fields).with_metadata(
                metadata
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        );
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.into_inner().unwrap()
    }
    fn one_list(members: &[Option<&str>]) -> arrow::array::ListArray {
        let mut lists = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        for member in members {
            lists.values().append_option(*member);
        }
        lists.append(true);
        lists.finish()
    }
    let key = || Arc::new(StringArray::from(vec!["a"])) as Arc<dyn Array>;
    let list = one_list(&[Some("QUFBQUFBQUFBQUE=")]);
    let list_type = list.data_type().clone();
    let (status, body) = patch_raw(
        &server,
        ARROW,
        stream(
            vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("members", list_type.clone(), true),
                Field::new("parent", DataType::Utf8, true),
            ],
            vec![key(), Arc::new(list.clone()), key()],
            &[("addressing", "external")],
        ),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("'parent'"),
        "{body}"
    );
    let (status, body) = patch_raw(
        &server,
        ARROW,
        stream(
            vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("members", list_type.clone(), true),
            ],
            vec![key(), Arc::new(list.clone())],
            &[("addressing", "external"), ("default_space", "wgs84")],
        ),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("`default_space`"),
        "{body}"
    );
    let null_members = {
        let mut lists = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        lists.append(false);
        lists.finish()
    };
    let (status, body) = patch_raw(
        &server,
        ARROW,
        stream(
            vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("members", null_members.data_type().clone(), true),
            ],
            vec![key(), Arc::new(null_members)],
            &[("addressing", "external")],
        ),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("column 'members' is null"),
        "{body}"
    );
    let (_, artifacts) = viewport(&server, &["0"], None).await;
    assert_eq!(
        artifacts.len(),
        2,
        "none of the three refusals applied anything"
    );

    // The Arrow form's envelope is the schema's metadata, and a stream without it is refused
    // naming what it lacks.
    let (status, body) = patch_raw(&server, ARROW, {
        let mut lists = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        lists.values().append_value(member(40));
        lists.append(true);
        let lists = lists.finish();
        let schema = Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("members", lists.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["a"])), Arc::new(lists)],
        )
        .unwrap();
        let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.into_inner().unwrap()
    })
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("addressing"),
        "{body}"
    );
}

/// A content type naming neither encoding is refused naming both, on every record-bearing
/// route; a publication under the Arrow type is refused naming the routes that take it.
#[tokio::test]
async fn a_content_type_naming_neither_encoding_is_refused() {
    let (_tmp, server) = served_plain(generous_ingest_limits()).await;
    let (status, body) = ingest(
        &server,
        "octets",
        Some("application/octet-stream"),
        arrow_body(&rows(0..1)),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("application/json") && detail.contains(ARROW),
        "{detail}"
    );

    let (status, body) = patch_raw(&server, "text/plain", grow_json(&[("p", members(0..1))])).await;
    assert_eq!(status, 422, "{body}");
    assert!(body["detail"].as_str().unwrap().contains(ARROW), "{body}");

    let (status, body) = put_raw(&server, ARROW, publish_body(&[("p", members(0..1))])).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("takes JSON"),
        "{body}"
    );
    assert_eq!(server.state.engine.published_artifacts(), 0);

    // `application/json; charset=utf-8` is JSON: the parameter is not part of the type.
    let (status, body) = put_raw(
        &server,
        "application/json; charset=utf-8",
        publish_body(&[("p", members(0..1))]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

/// A null coordinate in an Arrow batch is refused, as it is in JSON, and the batch has no effect.
#[tokio::test]
async fn a_null_coordinate_in_an_arrow_batch_is_refused() {
    let (_tmp, server) = served_declared().await;
    let high_water = control_status(&server).await["entity_id_high_water"].clone();
    for column in ["x", "y"] {
        let ids = [500u64, 501, 502];
        let access = access_column(ids.iter().map(|_| "0"));
        let schema = Arc::new(Schema::new(vec![
            Field::new("external_id", DataType::Binary, true),
            Field::new("x", DataType::Float64, true),
            Field::new("y", DataType::Float64, true),
            access_field(&access),
        ]));
        let with_null = |name: &str| -> Float64Array {
            if name == column {
                Float64Array::from(vec![Some(1.0), None, Some(3.0)])
            } else {
                Float64Array::from(vec![1.0, 2.0, 3.0])
            }
        };
        let external: Vec<Vec<u8>> = ids.iter().map(|id| external_id_of(*id)).collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(BinaryArray::from_iter_values(
                    external.iter().map(|v| v.as_slice()),
                )),
                Arc::new(with_null("x")),
                Arc::new(with_null("y")),
                Arc::new(access),
            ],
        )
        .unwrap();
        let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
        writer.write(&batch).unwrap();
        let body = writer.into_inner().unwrap();
        let (status, resp) = ingest(&server, &format!("null-{column}"), Some(ARROW), body).await;
        assert_eq!(status, 422, "{column}: {resp}");
    }
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water,
        "no refused batch allocated an entity"
    );
}

async fn declare(server: &TestServer, body: Value) {
    let resp = server
        .client
        .put(server.control_url("/control/attributes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(status == 200 || status == 201, "the declaration is accepted: {answer}");
}

/// Three attributes declared live whose Arrow columns below arrive at another width.
async fn declare_widths(server: &TestServer) {
    declare(server, json!({ "name": "level", "type": "u8", "index": true })).await;
    declare(server, json!({ "name": "precise", "type": "f64", "index": true })).await;
    declare(server, json!({ "name": "label", "type": "keyword", "index": true })).await;
}

/// An ingest batch whose declared columns are each at a width other than the declaration's:
/// `level` (`u8`) as `int64`, `weight` (`f32`) as `float64`, `precise` (`f64`) as `float32` and
/// `label` (`keyword`, a string on the wire) as `large_utf8`.
fn widths_body(ids: &[u64], levels: &[Option<i64>]) -> Vec<u8> {
    let access = access_column(ids.iter().map(|_| "0"));
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("level", DataType::Int64, true),
        Field::new("weight", DataType::Float64, true),
        Field::new("precise", DataType::Float32, true),
        Field::new("label", DataType::LargeUtf8, true),
    ]));
    let external: Vec<Vec<u8>> = ids.iter().map(|id| external_id_of(*id)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                external.iter().map(|v| v.as_slice()),
            )),
            Arc::new(Float32Array::from_iter_values(ids.iter().map(|_| 10.0))),
            Arc::new(Float32Array::from_iter_values(ids.iter().map(|_| 20.0))),
            Arc::new(access),
            Arc::new(Int64Array::from(levels.to_vec())),
            Arc::new(Float64Array::from_iter_values(ids.iter().map(|_| 0.5))),
            Arc::new(Float32Array::from_iter_values(ids.iter().map(|_| 2.5))),
            Arc::new(arrow::array::LargeStringArray::from_iter_values(
                ids.iter().map(|id| format!("wide-{id}")),
            )),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// One item's drill-down fields.
async fn fields_of(server: &TestServer, tessera_id: u64) -> Value {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = post_item(server, token, tessera_id).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    body["fields"].clone()
}

fn tessera_id_at(answer: &Value, row: usize) -> u64 {
    let id = &answer["tessera_ids"][row];
    id.as_u64()
        .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
        .expect("the ingest answers each row's tessera_id")
}

/// **An Arrow column is read by the rule a build reads a points file by**: any integer type
/// carries an integer declaration whose range holds its values, either float width carries
/// either float declaration, and a string at either offset width carries a string one. Each
/// value is served at its declared type.
#[tokio::test]
async fn an_arrow_column_at_another_width_is_read_as_a_build_reads_it() {
    let (_tmp, server) = served_declared().await;
    declare_widths(&server).await;
    let body = widths_body(&[600, 601], &[Some(7), Some(255)]);
    let (status, answer) = ingest(&server, "widths", Some(ARROW), body).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["accepted"], 2);
    flush(&server).await;

    let (matched, _) = viewport(&server, &["0"], Some(json!({ "level": { "eq": 255 } }))).await;
    assert_eq!(matched.len(), 1, "the u8 is filterable at its value");
    let fields = fields_of(&server, tessera_id_at(&answer, 0)).await;
    assert_eq!(fields["level"], json!(7));
    assert_eq!(fields["weight"], json!(0.5));
    assert_eq!(fields["precise"], json!(2.5));
    assert_eq!(fields["label"], json!("wide-600"));
}

/// An integer that does not fit its declaration is refused naming its row, and the batch has
/// no effect.
#[tokio::test]
async fn an_arrow_integer_outside_its_declaration_is_refused_naming_the_row() {
    let (_tmp, server) = served_declared().await;
    declare_widths(&server).await;
    let high_water = control_status(&server).await["entity_id_high_water"].clone();
    let (status, answer) = ingest(
        &server,
        "too-wide",
        Some(ARROW),
        widths_body(&[700, 701, 702], &[Some(7), Some(256), Some(9)]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
    assert_eq!(answer["error"], "contract");
    let detail = answer["detail"].as_str().unwrap();
    assert!(detail.contains("row 1, column 'level'"), "{detail}");
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water,
        "the refused batch allocated no entity"
    );
}

/// A values batch over an existing entity, as Arrow, filling `level` from an `int64` column.
fn level_values_body(external_id: u64, level: i64) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("level", DataType::Int64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values([external_id_of(external_id)])),
            Arc::new(Int64Array::from(vec![level])),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn post_values(server: &TestServer, batch_id: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", ARROW)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `/control/values` reads its Arrow columns by the same rule: an `int64` column fills a `u8`
/// attribute where the value fits and is refused where it does not.
#[tokio::test]
async fn an_arrow_values_column_at_another_width_is_read_as_a_build_reads_it() {
    let (_tmp, server) = served_declared().await;
    declare_widths(&server).await;
    let (status, answer) = ingest(&server, "rows", Some(ARROW), widths_body(&[800], &[None])).await;
    assert_eq!(status, 200, "{answer}");
    let tessera_id = tessera_id_at(&answer, 0);
    flush(&server).await;

    let (status, answer) = post_values(&server, "too-wide", level_values_body(800, 300)).await;
    assert_eq!(status, 422, "{answer}");
    let detail = answer["detail"].as_str().unwrap();
    assert!(detail.contains("row 0, column 'level'"), "{detail}");

    let (status, answer) = post_values(&server, "fits", level_values_body(800, 42)).await;
    assert_eq!(status, 200, "{answer}");
    flush(&server).await;
    assert_eq!(fields_of(&server, tessera_id).await["level"], json!(42));
}

/// A `uint64` past `i64::MAX`, which an `i64` cannot hold.
const PAST_I64: u64 = 1 << 63;

/// **A `uint64` past `i64::MAX` is out of range for `i64`, at both paths**, and is never wrapped
/// to a negative number: the service refuses the batch naming the row and the value as sent, and
/// allocates nothing; the build refuses the same column in a points file.
#[tokio::test]
async fn a_uint64_past_i64_max_is_refused_for_an_i64_at_both_paths() {
    let (_tmp, server) = served_declared().await;
    let high_water = control_status(&server).await["entity_id_high_water"].clone();
    let ids = [900u64, 901];
    let access = access_column(ids.iter().map(|_| "0"));
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::UInt64, true),
    ]));
    let external: Vec<Vec<u8>> = ids.iter().map(|id| external_id_of(*id)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                external.iter().map(|v| v.as_slice()),
            )),
            Arc::new(Float32Array::from(vec![10.0, 11.0])),
            Arc::new(Float32Array::from(vec![20.0, 21.0])),
            Arc::new(access),
            Arc::new(UInt64Array::from(vec![7, PAST_I64])),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    let body = writer.into_inner().unwrap();
    let (status, answer) = ingest(&server, "past-i64", Some(ARROW), body).await;
    assert_eq!(status, 422, "{answer}");
    let detail = answer["detail"].as_str().unwrap();
    assert!(detail.contains("row 1, column 'score'"), "{detail}");
    assert!(detail.contains(&PAST_I64.to_string()), "the value as sent: {detail}");
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water,
        "the refused batch allocated no entity"
    );

    let tmp = TempDir::new().unwrap();
    let points = tmp.path().join("points.parquet");
    write_points_scored(
        &points,
        Arc::new(UInt64Array::from_iter_values(
            (0..N).map(|e| if e == 3 { PAST_I64 } else { e }),
        )),
    );
    assert!(
        build_over(tmp.path(), points).is_err(),
        "the build refuses the same column"
    );
}

/// **A column's type is checked on every record batch, an empty one included**, as the build
/// checks it: a `utf8` column for the `f32` attribute `weight` is refused though no row carries a
/// value, at both routes.
#[tokio::test]
async fn a_wrong_typed_column_in_an_empty_batch_is_refused() {
    let (_tmp, server) = served_declared().await;
    let access = access_column(std::iter::empty());
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("weight", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(std::iter::empty::<&[u8]>())),
            Arc::new(Float32Array::from(Vec::<f32>::new())),
            Arc::new(Float32Array::from(Vec::<f32>::new())),
            Arc::new(access),
            Arc::new(StringArray::from(Vec::<&str>::new())),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    let (status, answer) =
        ingest(&server, "empty", Some(ARROW), writer.into_inner().unwrap()).await;
    assert_eq!(status, 422, "{answer}");
    assert_eq!(answer["error"], "contract", "{answer}");

    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("weight", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(std::iter::empty::<&[u8]>())),
            Arc::new(StringArray::from(Vec::<&str>::new())),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    let (status, answer) = post_values(&server, "empty", writer.into_inner().unwrap()).await;
    assert_eq!(status, 422, "{answer}");
    assert_eq!(answer["error"], "contract", "{answer}");
}
