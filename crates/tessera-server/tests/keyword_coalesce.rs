//! **A filter on an indexed keyword column answers the same set before and after the entity-space
//! coalesce lands** (filter-index §5.2; records §7), through the served surface: rows ingested
//! over the control plane, one flush each, filtered over `/v1/viewport`.
//!
//! The coalesce merges the window's dictionaries and renumbers every ordinal, so the served answer
//! stays the same only if the coalesced extent is read against the dictionary its merge wrote.
//! The engine's own tests (`tests/filtering.rs`) assert that through the column reader and the
//! evaluator; this one asserts it where a client would see it, and again after a restart opens the
//! coalesced extent from the manifest.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};

/// The one declared column: a keyword, indexed, so every flush writes an extent with its own
/// dictionary and the coalesce has a window to take.
const SCHEMA: &str = r#"
[[attribute]]
name  = "tag"
type  = "keyword"
index = true
"#;

/// The keys the window's rows carry, one per flush. Repeated and out of sorted order, so each
/// flush's dictionary numbers its key 0 and the merged dictionary numbers five keys another way.
const KEYS: [&str; 8] = [
    "delta", "alpha", "gamma", "alpha", "beta", "delta", "epsilon", "gamma",
];

/// The build's rows: `N` entities whose `tag` is `built-{e}`, so the base holds keys of its own
/// and every `prefix`/`contains` below reaches both the base and the window.
const N: u64 = 20;

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
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
            Arc::new(arrow::array::StringArray::from_iter_values(
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
    let pairs = dir.join("pairs.parquet");
    write_points(&points);
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
    })
    .expect("the build succeeds");
    out
}

/// One ingest row at the wire's shape: the reserved columns and `tag`.
fn batch(id: &[u8], x: f32, y: f32, tag: &str) -> Vec<u8> {
    let access = access_column(["0"]);
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("tag", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::BinaryArray::from_iter([Some(id)])),
            Arc::new(arrow::array::Float32Array::from_iter_values([x])),
            Arc::new(arrow::array::Float32Array::from_iter_values([y])),
            Arc::new(access),
            Arc::new(arrow::array::StringArray::from_iter_values([tag])),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(server: &TestServer, batch_id: &str, tag: &str, i: usize) {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .header("content-type", "application/octet-stream")
        .body(batch(
            batch_id.as_bytes(),
            100.0 + i as f32,
            100.0 + i as f32,
            tag,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{batch_id} lands");
}

/// Pull one tick and wait for the flush it publishes.
async fn flush(server: &TestServer) {
    let before = server.state.engine.write_executor_stats().flushes;
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while server.state.engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The `tessera_id`s a filtered full viewport serves.
async fn matched(server: &TestServer, token: &str, filter: Value) -> BTreeSet<u64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&json!({
                "view": "s0", "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
                "filters": filter.clone()
            }))
            .send()
            .await
            .unwrap();
        let unsettled = std::time::Instant::now() < deadline;
        if resp.status().as_u16() == 429 && unsettled {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        assert_eq!(resp.status().as_u16(), 200, "a filtered viewport answers");
        if resp
            .headers()
            .get("x-tessera-stale")
            .is_some_and(|v| v == "1")
            && unsettled
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        return decode_viewport(&resp.bytes().await.unwrap())
            .1
            .into_iter()
            .map(|(id, _)| id)
            .collect();
    }
}

/// Every keyword operator over `tag`, each with the set it serves.
async fn answers(server: &TestServer, token: &str) -> Vec<(String, BTreeSet<u64>)> {
    let mut filters: Vec<Value> = KEYS
        .iter()
        .map(|key| json!({ "tag": { "eq": key } }))
        .collect();
    filters.push(json!({ "tag": { "eq": "built-3" } }));
    filters.push(json!({ "tag": { "eq": "zeta" } }));
    filters.push(json!({ "tag": { "in": ["alpha", "epsilon", "built-1"] } }));
    filters.push(json!({ "tag": { "prefix": "" } }));
    filters.push(json!({ "tag": { "prefix": "a" } }));
    filters.push(json!({ "tag": { "prefix": "built-1" } }));
    filters.push(json!({ "tag": { "contains": "lta" } }));
    filters.push(json!({ "tag": { "contains": "t-1" } }));
    let mut out = Vec::new();
    for filter in filters {
        out.push((filter.to_string(), matched(server, token, filter).await));
    }
    out
}

/// This partition's `attr_extents` entries for `tag`, from the newest side-manifest.
fn tag_extents(root: &Path) -> Vec<Value> {
    let current: Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).unwrap()).unwrap();
    let partition = root
        .join(current["prefix"].as_str().expect("CURRENT names a prefix"))
        .join("partitions/default");
    let newest = std::fs::read_dir(&partition)
        .expect("the partition directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("SEGMENTS-"))
        })
        .max_by_key(|p| {
            p.file_stem()
                .and_then(|n| n.to_str())
                .and_then(|n| n.trim_start_matches("SEGMENTS-").parse::<u64>().ok())
                .unwrap_or(0)
        })
        .expect("a side-manifest");
    let manifest: Value = serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap();
    manifest["attr_extents"]
        .as_array()
        .expect("attr_extents")
        .iter()
        .filter(|e| e["column"] == "tag")
        .cloned()
        .collect()
}

/// **The served answer to every keyword operator is the same set before and after the coalesce,
/// and after a restart that opens the coalesced extent from the manifest.**
///
/// The pass is held off while the window is ingested, so the "before" capture is over eight
/// single-flush extents; one pulled tick with the pass enabled then collapses them. The default
/// width is eight, so the window is exactly the rows ingested.
#[tokio::test]
async fn a_keyword_filter_serves_the_same_set_before_and_after_the_coalesce() {
    let tmp = TempDir::new().unwrap();
    let root = build_bundle(tmp.path());
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    server.state.engine.set_coalesce_for_test(false);

    for (i, key) in KEYS.iter().enumerate() {
        ingest(&server, &format!("kw-{i}"), key, i).await;
        flush(&server).await;
    }
    let extents = tag_extents(&root);
    assert_eq!(
        extents.len(),
        KEYS.len(),
        "one extent per flush: {extents:?}"
    );
    assert!(
        extents.iter().all(|e| e["dict"].is_string()),
        "every flush extent names its own dictionary"
    );
    let before = answers(&server, &token).await;
    let (_, everything) = &before[KEYS.len() + 3];
    assert_eq!(
        everything.len() as u64,
        N + KEYS.len() as u64,
        "the empty prefix reaches the base and the whole window"
    );

    // The pass, on one pulled tick.
    let coalesces = server.state.engine.write_executor_stats().coalesces;
    server.state.engine.set_coalesce_for_test(true);
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while server.state.engine.write_executor_stats().coalesces == coalesces {
        assert!(
            std::time::Instant::now() < deadline,
            "the coalesce never published: {:?}",
            server.state.engine.write_executor_stats()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let extents = tag_extents(&root);
    assert_eq!(extents.len(), 1, "the window collapsed to one: {extents:?}");
    assert!(
        extents[0]["dict"].is_string(),
        "the coalesced extent names the merged dictionary beside its values: {extents:?}"
    );
    let after = answers(&server, &token).await;
    assert_eq!(after, before, "a served answer moved across the coalesce");

    // A restart opens the coalesced extent from the manifest entry.
    drop(server);
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let reopened = answers(&server, &token).await;
    assert_eq!(reopened, before, "a served answer moved across the restart");
}
