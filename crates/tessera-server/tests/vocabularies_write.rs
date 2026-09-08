//! **`PUT /control/vocabularies/{name}` and `PATCH /control/vocabularies/{name}/values`**
//! (`ingest.md` §1.3; decision 0136, track T5): a vocabulary is declared at a running service, its
//! values are paged, an identical redeclaration answers the vocabulary that exists, a differing
//! one is refused, a `declared` category column may then use its values end to end, and both the
//! declaration and its values survive a restart and a fold.
//!
//! What this file pins is the wire and the durability: the status codes and bodies the two routes
//! answer, the codes the server assigns and the caller never supplies, and the value a viewer
//! filters and resolves through `/v1/categories`.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};

const N: u64 = 40;

/// One rendered `f32` at the build and **no vocabulary at all**, so every vocabulary below is one
/// the running service declared.
const SCHEMA_TOML: &str = r#"
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
    let mut w =
        parquet::arrow::ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
            .unwrap();
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
    token: String,
    tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    build_fixture_bundle(tmp.path());
    open(tmp).await
}

async fn open(tmp: TempDir) -> Served {
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    Served { server, token, tmp }
}

/// Reopen the same bundle and the same log: the restart every durability claim below is made
/// against. The old server is dropped first so its executor releases the log.
async fn restart(served: Served) -> Served {
    let Served { server, tmp, .. } = served;
    drop(server);
    open(tmp).await
}

async fn declare(served: &Served, name: &str, body: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/vocabularies/{name}")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn page(served: &Served, name: &str, values: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .patch(
            served
                .server
                .control_url(&format!("/control/vocabularies/{name}/values")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "values": values }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn declare_attribute(served: &Served, body: Value) -> (u16, Value) {
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
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `severity`, a closed set of three, declared with its values inline.
fn severity() -> Value {
    json!({
        "value_set": "closed",
        "visibility": "public",
        "width": "u8",
        "values": [
            { "key": "low", "title": "Low" },
            { "key": "high", "title": "High" }
        ]
    })
}

/// The whole value set the server currently holds, by key, as `/v1/categories` pages it.
async fn categories(served: &Served, column: &str) -> Vec<(String, u64)> {
    let token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .get(
            served
                .server
                .viewer_url(&format!("/v1/categories/{column}")),
        )
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let body: Value = resp.json().await.unwrap();
    body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["key"].as_str().unwrap().to_string(),
                v["code"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// The vocabularies `/v1/meta` publishes, by name.
async fn meta_vocabularies(served: &Served) -> Vec<String> {
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let meta: Value = resp.json().await.unwrap();
    meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["category"]["vocabulary"].as_str().map(str::to_string))
        .collect()
}

/// One ingest row: the external id, the build's `score`, and the category key where it carries
/// one.
fn batch(rows: &[(&'static str, f32, Option<&'static str>)], column: bool) -> Vec<u8> {
    let labels: Vec<&[&str]> = rows.iter().map(|_| &["0"][..]).collect();
    let access = access_lists(&labels);
    let mut fields = vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::Float32, true),
    ];
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(arrow::array::BinaryArray::from_iter(
            rows.iter().map(|r| Some(r.0.as_bytes())),
        )),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(access),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|r| r.1))),
    ];
    if column {
        fields.push(Field::new("severity", DataType::Utf8, true));
        columns.push(Arc::new(StringArray::from_iter(rows.iter().map(|r| r.2))));
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(served: &Served, batch_id: &str, body: Vec<u8>) -> (u16, Value) {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn flush(served: &Served) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
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
        while served.server.state.engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if served.server.state.engine.buffered_items() == 0 {
            break;
        }
    }
}

/// Request a compaction fold and block until it has published (contracts §3.4). The counter is
/// the only "done" there is: the fold runs on its own thread and publishes at the executor's next
/// loop iteration.
async fn fold(served: &Served) {
    let before = served.server.state.engine.write_executor_stats().folds;
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "a fold is accepted at any time");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let stats = served.server.state.engine.write_executor_stats();
        assert_eq!(
            stats.fold_failures, 0,
            "the fold failed rather than publishing"
        );
        if stats.folds > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The `tessera_id`s a filtered viewport answers, from a fresh session so the rows flushed since
/// the last one are in the answer.
async fn filtered(served: &Served, filters: Value) -> BTreeSet<u64> {
    let token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": filters
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

// ---------------------------------------------------------------------------------------------

/// **The declaration route's answers** (contracts §3.4): `201` for a new vocabulary, `200` for an
/// identical redeclaration, `409` for a held name under another identity, and `422` for a
/// declaration the schema's rules refuse. The codes in the answer are the server's: the request
/// names none, and a request that tries to is refused rather than read past.
#[tokio::test]
async fn the_route_declares_answers_redeclarations_and_refuses_what_the_schema_refuses() {
    let served = serve().await;

    let (status, body) = declare(&served, "severity", severity()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body,
        json!({ "name": "severity", "existing": false, "added": 2 })
    );

    let (status, body) = declare(&served, "severity", severity()).await;
    assert_eq!(status, 200, "identical: the vocabulary that exists: {body}");
    assert_eq!(
        body,
        json!({ "name": "severity", "existing": true, "added": 0 }),
        "a repeated value is a no-op"
    );

    let mut wider = severity();
    wider["width"] = json!("u16");
    let (status, body) = declare(&served, "severity", wider).await;
    assert_eq!(status, 409, "a held name at another width: {body}");
    assert_eq!(body["error"], "conflict");

    let mut opened = severity();
    opened["value_set"] = json!("open");
    let (status, _) = declare(&served, "severity", opened).await;
    assert_eq!(status, 409, "and under another value set");

    for (bad, reason) in [
        (
            json!({ "value_set": "closed", "visibility": "public", "width": "f32",
                    "values": [{ "key": "a" }] }),
            "is not a code space",
        ),
        (
            json!({ "value_set": "closed", "visibility": "public", "width": "u8" }),
            "with no values",
        ),
        (
            json!({ "value_set": "open", "visibility": "derived", "width": "u8",
                    "reserved": [0] }),
            "code 0",
        ),
        (
            json!({ "value_set": "open", "visibility": "derived", "width": "u8",
                    "reserved": [900] }),
            "cannot hold",
        ),
        (
            json!({ "value_set": "open", "visibility": "derived", "width": "u8",
                    "values": [{ "key": "" }] }),
            "empty key",
        ),
        (
            json!({ "value_set": "open", "visibility": "derived", "width": "u8",
                    "values": [{ "key": "a" }, { "key": "a" }] }),
            "named twice",
        ),
    ] {
        let (status, body) = declare(&served, "other", bad.clone()).await;
        assert_eq!(status, 422, "{bad}: {body}");
        assert!(
            body["detail"].as_str().unwrap().contains(reason),
            "{bad}: {body}"
        );
    }

    // **A code is never the caller's** (per-point-attributes §3.1): the body has no field for one,
    // so a request naming one is refused by the shape rather than read with the code dropped.
    let (status, _) = declare(
        &served,
        "pinned",
        json!({ "value_set": "closed", "visibility": "public", "width": "u8",
                "values": [{ "key": "low", "code": 3 }] }),
    )
    .await;
    assert_eq!(status, 422, "a caller-supplied code is refused");

    // A name outside the column charset, which is what a value key is addressed by on the wire.
    let (status, body) = declare(&served, "sev erity", severity()).await;
    assert_eq!(status, 422, "{body}");
}

/// **A value page adds values, a repeat adds nothing, an absent title is filled and a differing
/// one is refused** (`ingest.md` §1.1's three arms), and a page against a vocabulary this
/// deployment does not carry is the same `404` an unknown view is.
#[tokio::test]
async fn a_value_page_adds_values_and_a_held_property_is_filled_or_refused() {
    let served = serve().await;
    assert_eq!(declare(&served, "severity", severity()).await.0, 201);

    let (status, body) = page(
        &served,
        "severity",
        json!([{ "key": "medium" }, { "key": "critical", "title": "Critical" }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["added"], 2);
    assert_eq!(body["existing"], 0);

    let (status, body) = page(&served, "severity", json!([{ "key": "medium" }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["added"], 0, "a repeated value is a no-op");
    assert_eq!(body["existing"], 1);

    // Absent, so filled.
    let (status, body) = page(
        &served,
        "severity",
        json!([{ "key": "medium", "title": "Medium" }]),
    )
    .await;
    assert_eq!(
        status, 200,
        "a title on a value that has none fills it: {body}"
    );
    assert_eq!(body["added"], 0);

    // Held differently, so refused.
    let (status, body) = page(
        &served,
        "severity",
        json!([{ "key": "medium", "title": "Middling" }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("different title"),
        "{body}"
    );

    let (status, body) = page(&served, "nothing", json!([{ "key": "a" }])).await;
    assert_eq!(status, 404, "{body}");

    // A page carries no code either.
    let (status, _) = page(&served, "severity", json!([{ "key": "x", "code": 9 }])).await;
    assert_eq!(status, 422);
}

/// **A `declared` category column uses a runtime-declared vocabulary's values end to end**: the
/// column is declared over it, a batch carries a value key, the filter answers the rows that
/// carry it, `/v1/categories` resolves the codes the server assigned, and a key the vocabulary
/// does not hold is the declare-then-use refusal (per-point-attributes §5).
#[tokio::test]
async fn a_declared_category_column_uses_a_runtime_vocabularys_values() {
    let served = serve().await;
    assert_eq!(declare(&served, "severity", severity()).await.0, 201);
    let (status, body) = declare_attribute(
        &served,
        json!({ "name": "severity", "type": "category", "vocabulary": "severity",
                "width": "u8", "index": true }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(meta_vocabularies(&served).await, ["severity"]);

    // A key the vocabulary does not hold: the whole batch is refused, and nothing is minted.
    let (status, body) = ingest(
        &served,
        "unknown-key",
        batch(&[("u1", 1.0, Some("nonsense"))], true),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let held: Vec<String> = categories(&served, "severity")
        .await
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(held, ["high", "low"], "the refused key minted nothing");

    let (status, body) = ingest(
        &served,
        "carrying",
        batch(&[("c1", 1.0, Some("high")), ("c2", 2.0, Some("low"))], true),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ids: Vec<u64> = body["tessera_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap()
        })
        .collect();
    flush(&served).await;

    let high = filtered(
        &served,
        json!({ "all_of": [{ "severity": { "in": ["high"] } }] }),
    )
    .await;
    assert_eq!(
        high,
        BTreeSet::from([ids[0]]),
        "the filter answers exactly the row carrying the runtime-declared value"
    );

    // A value's code is the server's, and `/v1/categories` is where a client learns it.
    let values = categories(&served, "severity").await;
    let keys: Vec<&str> = values.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["high", "low"]);
    assert!(
        values.iter().all(|(_, code)| *code > 0 && *code <= 255),
        "every code is drawn inside the declared u8 space: {values:?}"
    );
}

/// **The declaration and its values survive a restart and a fold** (`ingest.md` §1.3): from the
/// log alone before any publication, from the segments manifest after one, and from
/// `MANIFEST.json` after the fold that writes them there.
#[tokio::test]
async fn a_declaration_and_its_values_survive_a_restart_and_a_fold() {
    let served = serve().await;
    assert_eq!(declare(&served, "severity", severity()).await.0, 201);
    assert_eq!(
        page(
            &served,
            "severity",
            json!([{ "key": "medium", "title": "Medium" }])
        )
        .await
        .0,
        200
    );
    assert_eq!(
        declare_attribute(
            &served,
            json!({ "name": "severity", "type": "category", "vocabulary": "severity",
                    "width": "u8", "index": true }),
        )
        .await
        .0,
        201
    );
    let before = categories(&served, "severity").await;
    assert_eq!(before.len(), 3);

    // Replayed from the log, nothing having been published yet.
    let served = restart(served).await;
    assert_eq!(
        categories(&served, "severity").await,
        before,
        "every key keeps the code it was assigned"
    );
    assert_eq!(
        declare(&served, "severity", severity()).await.0,
        200,
        "the replayed vocabulary is the one a redeclaration meets"
    );

    // Published into a segments manifest, then replayed from it.
    assert_eq!(
        ingest(&served, "rows", batch(&[("c1", 1.0, Some("high"))], true))
            .await
            .0,
        200
    );
    flush(&served).await;
    let served = restart(served).await;
    assert_eq!(
        categories(&served, "severity").await,
        before,
        "carried by the segments manifest"
    );

    // Folded into `MANIFEST.json`, then replayed from it.
    fold(&served).await;
    assert_eq!(categories(&served, "severity").await, before);
    let served = restart(served).await;
    assert_eq!(
        categories(&served, "severity").await,
        before,
        "carried by MANIFEST.json"
    );
    assert_eq!(
        declare(&served, "severity", severity()).await.0,
        200,
        "and it is still the vocabulary a redeclaration meets"
    );
    assert_eq!(
        filtered(
            &served,
            json!({ "all_of": [{ "severity": { "in": ["high"] } }] })
        )
        .await
        .len(),
        1,
        "the column still answers over the folded vocabulary"
    );
}
