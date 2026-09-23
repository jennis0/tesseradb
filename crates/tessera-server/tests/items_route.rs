//! `POST /v1/items` over HTTP: a read carried across responses by its cursor returns every row a
//! viewer may see once, the head's counts are the viewport's, compression changes the bytes and
//! not the rows, every refusal has its status, a cursor is bound to the credential it was issued
//! to, the bulk-read lane and the viewport's hold no slot of each other's, a client that goes away
//! frees its slot, the response budgets end a response that the next one resumes, and `/v1/meta`
//! publishes what a caller needs to ask.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Float32Array, Float64Array, Int32Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};
use tessera_server::state::{ComputeGate, ServeLimits};

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
name   = "score"
type   = "f32"
render = true

[[attribute]]
name  = "year"
type  = "i32"
index = true

[[attribute]]
name = "note"
type = "keyword"
"#;

/// The fixture's items: `archive` rendered, `score` rendered, `year` indexed and `note` held in
/// the record store alone, `note_bytes` long.
fn write_points(path: &Path, n: u64, note_bytes: usize) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, false),
        Field::new("score", DataType::Float32, true),
        Field::new("year", DataType::Int32, true),
        Field::new("note", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<&str> = ids
        .iter()
        .map(|&e| ["astro", "cond", "hep"][(e % 3) as usize])
        .collect();
    let scores: Vec<Option<f32>> = ids.iter().map(|&e| Some((e % 97) as f32 * 0.5)).collect();
    let years: Vec<Option<i32>> = ids
        .iter()
        .map(|&e| (e % 7 != 0).then_some(1990 + (e % 30) as i32))
        .collect();
    let notes: Vec<Option<String>> = ids
        .iter()
        .map(|&e| {
            (e % 11 != 0).then(|| {
                let mut note = format!("note-{e}-");
                while note.len() < note_bytes {
                    note.push((b'a' + (note.len() % 26) as u8) as char);
                }
                note
            })
        })
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(Float32Array::from(scores)),
            Arc::new(Int32Array::from(years)),
            Arc::new(StringArray::from(notes)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_bundle(tmp: &TempDir, n: u64, note_bytes: usize) -> std::path::PathBuf {
    let out = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points, n, note_bytes);
    write_pairs_n(&pairs, n);
    let schema_path = tmp.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .unwrap()
        .schema;
    let args = BuildArgs {
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
    };
    build(&args).expect("fixture build should succeed");
    out
}

const N: u64 = 3_000;

struct Fixture {
    _tmp: TempDir,
    server: TestServer,
}

/// A server over the fixture with the gates and limits the test chooses.
async fn fixture_with(
    n: u64,
    note_bytes: usize,
    compute_gate: ComputeGate,
    bulk_gate: ComputeGate,
    tune: impl FnOnce(&mut ServeLimits),
) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let bundle = build_bundle(&tmp, n, note_bytes);
    let server = spawn_server_with_bulk_reads(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        compute_gate,
        bulk_gate,
        tune,
    )
    .await;
    Fixture { _tmp: tmp, server }
}

async fn fixture() -> Fixture {
    fixture_with(N, 16, generous_test_gate(), generous_bulk_gate(), |_| {}).await
}

async fn token(server: &TestServer, terms: &[&str]) -> String {
    authorise(server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn post_items(server: &TestServer, token: &str, body: &Value) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url("/v1/items"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// One response, which must be a 200, decoded.
async fn items_ok(server: &TestServer, token: &str, body: &Value) -> DecodedItems {
    let resp = post_items(server, token, body).await;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    decode_items(&bytes)
}

/// A whole read: `body` sent, then again with each response's `next` as its cursor until that is
/// null. Returns every response.
async fn read_all(server: &TestServer, token: &str, body: &Value) -> Vec<DecodedItems> {
    let mut responses = Vec::new();
    let mut body = body.clone();
    loop {
        let decoded = items_ok(server, token, &body).await;
        let next = decoded.trailer["next"].clone();
        responses.push(decoded);
        assert!(responses.len() < 10_000, "a read that never ends");
        match next {
            Value::Null => return responses,
            Value::String(cursor) => {
                body["cursor"] = json!(cursor);
                // `count` is refused with a cursor.
                body.as_object_mut().unwrap().remove("count");
            }
            other => panic!("next is {other}"),
        }
    }
}

fn ids_of(responses: &[DecodedItems]) -> Vec<u64> {
    responses.iter().flat_map(DecodedItems::tessera_ids).collect()
}

/// The viewport's visible and matched counts over the whole view.
async fn viewport_counts(server: &TestServer, token: &str, filters: Option<&Value>) -> (u64, u64) {
    let mut body = json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 });
    if let Some(filters) = filters {
        body["filters"] = filters.clone();
    }
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    decoded
        .tiles
        .iter()
        .fold((0, 0), |(v, m), (_, visible, matched)| (v + visible, m + matched))
}

/// Both permits of `gate`, once the request before has let them go.
async fn hold(gate: &ComputeGate) -> tessera_server::state::GatePermits {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match gate.admit().await {
            Ok((permits, _)) => return permits,
            Err(_) => {
                assert!(Instant::now() < deadline, "the gate never came free");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// The error envelope's code, after the status.
async fn refused(resp: reqwest::Response, status: u16) -> String {
    assert_eq!(resp.status().as_u16(), status);
    let body: Value = resp.json().await.expect("a refusal carries the envelope");
    body["error"].as_str().unwrap().to_string()
}

/// **Every visible row once, in either order, filtered or not.** A read carried across many
/// responses, three pages of 97 rows each, returns as many rows as the viewport counts, none
/// twice, and the two orders return the same set.
#[tokio::test]
async fn a_full_read_returns_every_visible_row_once_in_either_order() {
    let f = fixture().await;
    let filter = json!({ "score": { "range": { "gte": 30 } } });
    for terms in [&["0"][..], &["1"][..]] {
        let token = token(&f.server, terms).await;
        for filters in [None, Some(&filter)] {
            let (visible, matched) = viewport_counts(&f.server, &token, filters).await;
            if filters.is_some() {
                assert!(matched > 0 && matched < visible);
            }
            let mut sets = Vec::new();
            for order in ["map", "stored"] {
                let mut body = json!({
                    "view": "s0", "fields": ["score", "note"], "order": order,
                    "page_rows": 97, "pages": 3,
                });
                if let Some(filters) = filters {
                    body["filters"] = filters.clone();
                }
                let responses = read_all(&f.server, &token, &body).await;
                assert!(responses.len() > 1, "the read spans several responses");
                for response in &responses {
                    assert_eq!(response.head["order"], order);
                    assert_eq!(response.head["page_rows"], 97);
                }
                let ids = ids_of(&responses);
                let set: HashSet<u64> = ids.iter().copied().collect();
                assert_eq!(set.len(), ids.len(), "a row returned twice ({terms:?}, {order})");
                assert_eq!(
                    ids.len() as u64,
                    if filters.is_some() { matched } else { visible },
                    "the read and the viewport disagree ({terms:?}, {order}, {filters:?})"
                );
                sets.push(set);
            }
            assert_eq!(sets[0], sets[1], "the two orders return different rows");
        }
    }
}

/// **The columns are the ones asked for, in the order asked for**: `tessera_id`, the named fields,
/// the system fields, then `tessera:matched` under `keep_unmatched`, which then returns every
/// visible row.
#[tokio::test]
async fn the_page_schema_is_the_named_fields_then_the_system_fields() {
    let f = fixture().await;
    let token = token(&f.server, &["1"]).await;
    let (visible, matched) =
        viewport_counts(&f.server, &token, Some(&json!({ "archive": { "in": ["astro"] } }))).await;
    let responses = read_all(
        &f.server,
        &token,
        &json!({
            "view": "s0",
            "fields": ["note", "archive", "year", "score"],
            "system_fields": ["labels", "position", "external_id"],
            "filters": { "archive": { "in": ["astro"] } },
            "keep_unmatched": true,
            "page_rows": 400,
        }),
    )
    .await;
    let (batch, _) = &responses[0].pages[0];
    let names: Vec<&str> = batch
        .schema_ref()
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "tessera_id",
            "note",
            "archive",
            "year",
            "score",
            "tessera:labels",
            "tessera:x",
            "tessera:y",
            "tessera:external_id",
            "tessera:matched",
        ]
    );
    let mut rows = 0u64;
    let mut marked = 0u64;
    for response in &responses {
        for (batch, _) in &response.pages {
            rows += batch.num_rows() as u64;
            let flags = batch
                .column_by_name("tessera:matched")
                .unwrap()
                .as_any()
                .downcast_ref::<arrow::array::BooleanArray>()
                .unwrap();
            marked += flags.true_count() as u64;
        }
    }
    assert_eq!(rows, visible, "keep_unmatched returns every visible row");
    assert_eq!(marked, matched, "and marks the matching ones");
}

/// **`count` puts the viewport's own counts in the head**, and the headers are the viewport's:
/// the same identity coordinate, and the region verdict exactly when a region leaf was sent.
#[tokio::test]
async fn count_puts_the_viewports_counts_in_the_head() {
    let f = fixture().await;
    let token = token(&f.server, &["1"]).await;
    let filter = json!({ "all_of": [
        { "archive": { "in": ["astro", "hep"] } },
        { "region": { "bbox": [100.0, 100.0, 800.0, 900.0] } },
    ] });
    let (visible, matched) = viewport_counts(&f.server, &token, Some(&filter)).await;
    let resp = post_items(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": [], "filters": filter, "count": true, "pages": 1 }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let items_identity = resp.headers()["x-tessera-identity-key"].clone();
    assert_eq!(resp.headers()["x-tessera-region"], "exact");
    for header in ["x-tessera-server-us", "x-tessera-admission-us"] {
        assert!(resp.headers()[header]
            .to_str()
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_digit()));
    }
    let decoded = decode_items(&resp.bytes().await.unwrap());
    assert_eq!(decoded.head["visible"], json!(visible));
    assert_eq!(decoded.head["matched"], json!(matched));

    let viewport = f
        .server
        .client
        .post(f.server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(viewport.headers()["x-tessera-identity-key"], items_identity);

    // No counts unless asked, and no region header without a region leaf.
    let resp = post_items(&f.server, &token, &json!({ "view": "s0", "fields": [], "pages": 1 })).await;
    assert!(resp.headers().get("x-tessera-region").is_none());
    let decoded = decode_items(&resp.bytes().await.unwrap());
    assert!(decoded.head.get("visible").is_none() && decoded.head.get("matched").is_none());
}

/// **zstd changes the bytes and not the rows**: the same read with and without it decodes to the
/// same batches, page for page, and the compressed pages are smaller.
#[tokio::test]
async fn zstd_pages_decode_to_the_uncompressed_pages() {
    let f = fixture().await;
    let token = token(&f.server, &["0"]).await;
    let body = json!({
        "view": "s0", "fields": ["note", "archive", "score", "year"],
        "system_fields": ["position", "external_id", "labels"],
        "order": "stored", "page_rows": 500,
    });
    let plain = read_all(&f.server, &token, &body).await;
    let mut zstd_body = body.clone();
    zstd_body["compression"] = json!("zstd");
    let zstd = read_all(&f.server, &token, &zstd_body).await;
    let batches = |responses: &[DecodedItems]| -> Vec<RecordBatch> {
        responses
            .iter()
            .flat_map(|r| r.pages.iter().map(|(batch, _)| batch.clone()))
            .collect()
    };
    assert_eq!(batches(&plain), batches(&zstd));
    let size = |responses: &[DecodedItems]| -> usize {
        responses
            .iter()
            .flat_map(|r| r.records_payloads.iter().map(Vec::len))
            .sum()
    };
    assert!(size(&zstd) < size(&plain), "{} against {}", size(&zstd), size(&plain));
}

/// **Every refusal has its status and code**, decided before any row is sent.
#[tokio::test]
async fn every_refusal_has_its_status() {
    let f = fixture().await;
    let token = token(&f.server, &["0"]).await;
    let first = items_ok(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": [], "order": "map", "page_rows": 10, "pages": 1 }),
    )
    .await;
    let cursor = first.trailer["next"].as_str().unwrap().to_string();
    // A character in the middle, so the change is to the sealed bytes and not to the padding
    // bits of the last character.
    let mut tampered = cursor.clone().into_bytes();
    let middle = tampered.len() / 2;
    tampered[middle] = if tampered[middle] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();

    let contract = [
        json!({ "view": "s0", "fields": ["no_such_field"] }),
        json!({ "view": "s0", "fields": ["score", "score"] }),
        json!({ "view": "s0", "fields": [], "system_fields": ["entity_id"] }),
        json!({ "view": "s0", "fields": [], "page_rows": 0 }),
        json!({ "view": "s0", "fields": [], "pages": 0 }),
        json!({ "view": "s0", "fields": [], "count": true, "cursor": cursor }),
        json!({ "view": "s0", "fields": [], "order": "random" }),
        json!({ "view": "s0", "fields": [], "compression": "gzip" }),
        json!({ "view": "s0", "fields": [], "unknown": 1 }),
        json!({ "view": "s0" }),
        json!({ "view": "s0", "fields": [], "filters": { "no_such_column": { "eq": 1 } } }),
        json!({ "view": "s0", "fields": [], "cursor": tampered }),
        json!({ "view": "s0", "fields": [], "cursor": "not a cursor" }),
        json!({ "view": "s0", "fields": [], "cursor": cursor, "order": "stored" }),
    ];
    for body in contract {
        let resp = post_items(&f.server, &token, &body).await;
        assert_eq!(refused(resp, 422).await, "contract", "{body}");
    }
    // The cursor itself still opens, so the refusals above are about what was changed.
    items_ok(&f.server, &token, &json!({ "view": "s0", "fields": [], "cursor": cursor })).await;

    let resp = post_items(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": [], "idset": FIXTURE_IDSET + 1 }),
    )
    .await;
    assert_eq!(refused(resp, 409).await, "conflict");
    let resp = post_items(&f.server, &token, &json!({ "view": "nowhere", "fields": [] })).await;
    assert_eq!(refused(resp, 404).await, "unknown");
    let resp = f
        .server
        .client
        .post(f.server.viewer_url("/v1/items"))
        .json(&json!({ "view": "s0", "fields": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused(resp, 401).await, "bad-credential");
    let resp = post_items(&f.server, "not-a-token", &json!({ "view": "s0", "fields": [] })).await;
    assert_eq!(refused(resp, 401).await, "bad-credential");
}

/// An expired session is refused as it is on every viewer route.
#[tokio::test]
async fn an_expired_session_is_refused() {
    let tmp = TempDir::new().unwrap();
    let bundle = build_bundle(&tmp, 64, 16);
    let mut config = default_engine_config();
    config.token_max_lifetime_secs = 0;
    let server = spawn_server_with_config(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;
    let token = token(&server, &["0"]).await;
    let resp = post_items(&server, &token, &json!({ "view": "s0", "fields": [] })).await;
    assert_eq!(refused(resp, 403).await, "expired-token");
}

/// **A cursor opens only for the credential it was issued to.** Another session with the same
/// credential resumes the read; a session with another credential is refused, with the cursor's
/// one refusal.
#[tokio::test]
async fn a_cursor_does_not_open_for_another_credential() {
    let f = fixture().await;
    let broad = token(&f.server, &["0"]).await;
    let first = items_ok(
        &f.server,
        &broad,
        &json!({ "view": "s0", "fields": [], "page_rows": 10, "pages": 1 }),
    )
    .await;
    let cursor = first.trailer["next"].as_str().unwrap().to_string();
    let body = json!({ "view": "s0", "fields": [], "cursor": cursor, "pages": 1 });

    let same_credential = token(&f.server, &["0"]).await;
    assert_ne!(same_credential, broad);
    let resumed = items_ok(&f.server, &same_credential, &body).await;
    assert!(!resumed.pages.is_empty());

    let narrow = token(&f.server, &["1"]).await;
    let resp = post_items(&f.server, &narrow, &body).await;
    assert_eq!(refused(resp, 422).await, "contract");
}

/// **The bulk-read lane and the viewport's hold no slot of each other's.** With the bulk lane
/// full, a viewport is served at once and another bulk read is shed; with the viewport's gate
/// full, a bulk read is served and another viewport is shed.
#[tokio::test]
async fn the_bulk_lane_and_the_viewport_gate_do_not_hold_each_other() {
    let f = fixture_with(
        N,
        16,
        ComputeGate::new(1, 0, 250),
        ComputeGate::for_bulk_reads(1),
        |_| {},
    )
    .await;
    let token = token(&f.server, &["0"]).await;
    let items_body = json!({ "view": "s0", "fields": ["score"], "pages": 1 });

    let held = hold(&f.server.state.bulk_gate).await;
    let started = Instant::now();
    viewport_counts(&f.server, &token, None).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a viewport waited on the bulk lane"
    );
    let resp = post_items(&f.server, &token, &items_body).await;
    assert!(resp.headers().contains_key("retry-after"));
    assert_eq!(refused(resp, 429).await, "backpressure");
    drop(held);

    let held = hold(&f.server.state.compute_gate).await;
    items_ok(&f.server, &token, &items_body).await;
    let resp = f
        .server
        .client
        .post(f.server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused(resp, 429).await, "backpressure");
    drop(held);
}

/// **A bulk read holds its compute permit for its whole response**, not only until its head: a
/// read whose reader has stopped still counts as running, and a second read is refused at once.
#[tokio::test]
async fn a_bulk_read_holds_its_compute_for_the_whole_response() {
    let f = fixture_with(
        2_000,
        12 * 1024,
        generous_test_gate(),
        ComputeGate::for_bulk_reads(1),
        |limits| {
            limits.stream_write_stall_ms = 120_000;
        },
    )
    .await;
    let token = token(&f.server, &["0"]).await;
    let mut reader = post_items(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": ["note"], "order": "stored", "page_rows": 10 }),
    )
    .await;
    assert_eq!(reader.status().as_u16(), 200);
    assert!(reader.chunk().await.unwrap().is_some());

    let bulk = control_status(&f.server).await["bulk"].clone();
    assert_eq!(bulk["in_flight"], 1, "the unread read holds its compute: {bulk}");
    assert_eq!(bulk["waiting"], 0, "{bulk}");
    let started = Instant::now();
    let resp = post_items(&f.server, &token, &json!({ "view": "s0", "fields": [], "pages": 1 })).await;
    assert!(resp.headers().contains_key("retry-after"));
    assert_eq!(refused(resp, 429).await, "backpressure");
    assert!(started.elapsed() < Duration::from_secs(5), "the refusal waited for admission");
    drop(reader);
}

/// **`/control/status`'s `bulk` block reports the bulk-read limit**: its admission, no queue, the
/// reads running and the refusals it made, apart from the viewport's `compute` block.
#[tokio::test]
async fn control_status_reports_the_bulk_read_limit() {
    let f = fixture_with(N, 16, generous_test_gate(), ComputeGate::for_bulk_reads(3), |_| {}).await;
    let bulk = control_status(&f.server).await["bulk"].clone();
    assert_eq!(bulk["admission"], 3, "{bulk}");
    assert_eq!(bulk["queue"], 0, "{bulk}");
    assert_eq!(bulk["in_flight"], 0, "{bulk}");
    assert_eq!(bulk["shed_total"], 0, "{bulk}");

    let token = token(&f.server, &["0"]).await;
    let held: Vec<_> = [
        hold(&f.server.state.bulk_gate).await,
        hold(&f.server.state.bulk_gate).await,
        hold(&f.server.state.bulk_gate).await,
    ]
    .into();
    let resp = post_items(&f.server, &token, &json!({ "view": "s0", "fields": [] })).await;
    assert_eq!(refused(resp, 429).await, "backpressure");
    let status = control_status(&f.server).await;
    assert_eq!(status["bulk"]["in_flight"], 3, "{status}");
    assert_eq!(status["bulk"]["shed_total"], 1, "{status}");
    assert_eq!(status["compute"]["shed_total"], 0, "{status}");
    drop(held);
}

/// **A client that disconnects mid-read frees its bulk slot.** The response is far larger than
/// the sockets buffer, so its producer is blocked on the reader while the slot is held, and a
/// second read is shed; once the reader goes, a read is served well inside the stall budget the
/// producer would otherwise wait out.
#[tokio::test]
async fn a_client_that_disconnects_frees_its_bulk_slot() {
    let f = fixture_with(
        2_000,
        12 * 1024,
        generous_test_gate(),
        ComputeGate::for_bulk_reads(1),
        |limits| {
            limits.stream_write_stall_ms = 120_000;
        },
    )
    .await;
    let token = token(&f.server, &["0"]).await;
    let mut reader = post_items(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": ["note"], "order": "stored", "page_rows": 10 }),
    )
    .await;
    assert_eq!(reader.status().as_u16(), 200);
    assert!(reader.chunk().await.unwrap().is_some());

    let small = json!({ "view": "s0", "fields": [], "pages": 1 });
    let resp = post_items(&f.server, &token, &small).await;
    assert_eq!(
        refused(resp, 429).await,
        "backpressure",
        "the unread response still holds the lane"
    );

    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let resp = post_items(&f.server, &token, &small).await;
        if resp.status().as_u16() == 200 {
            break;
        }
        assert_eq!(resp.status().as_u16(), 429);
        assert!(
            Instant::now() < deadline,
            "the slot was not freed after the client went away"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **The byte budget ends a response, which the next resumes.** Pages end by bytes, the response
/// by its budget with a cursor, and the read carried on from it returns every row once.
#[tokio::test]
async fn the_byte_budget_ends_a_response_that_the_next_resumes() {
    let page_bytes = 4096;
    let f = fixture_with(N, 64, generous_test_gate(), generous_bulk_gate(), |limits| {
        limits.max_page_bytes = page_bytes;
        limits.bulk_response_bytes = 3 * page_bytes;
    })
    .await;
    let token = token(&f.server, &["1"]).await;
    let (visible, _) = viewport_counts(&f.server, &token, None).await;
    let body = json!({ "view": "s0", "fields": ["note"], "page_rows": 1000 });
    let first = items_ok(&f.server, &token, &body).await;
    assert_eq!(first.trailer["ended_by"], "budget_bytes");
    assert!(first.trailer["next"].is_string());
    assert!(first.pages.len() >= 2);
    for (_, end) in &first.pages {
        assert_eq!(end["ended_by"], "bytes");
    }
    let ids = ids_of(&read_all(&f.server, &token, &body).await);
    assert_eq!(ids.iter().copied().collect::<HashSet<_>>().len(), ids.len());
    assert_eq!(ids.len() as u64, visible);
}

/// **The time budget ends a response, which the next resumes**, and every response moves the
/// read on: at a budget of nothing, each response still carries rows and the read completes.
#[tokio::test]
async fn the_time_budget_ends_a_response_that_the_next_resumes() {
    let f = fixture_with(N, 16, generous_test_gate(), generous_bulk_gate(), |limits| {
        limits.bulk_response_ms = 0;
    })
    .await;
    let token = token(&f.server, &["1"]).await;
    let (visible, _) = viewport_counts(&f.server, &token, None).await;
    let body = json!({ "view": "s0", "fields": ["score"], "page_rows": 200 });
    let responses = read_all(&f.server, &token, &body).await;
    assert!(responses.len() > 1);
    for response in &responses[..responses.len() - 1] {
        assert_eq!(response.trailer["ended_by"], "budget_time");
    }
    let ids = ids_of(&responses);
    assert_eq!(ids.iter().copied().collect::<HashSet<_>>().len(), ids.len());
    assert_eq!(ids.len() as u64, visible);
}

/// **The stream deadline cancels the engine rather than cutting the body**: a response that
/// outlives it ends with a trailer saying so, and a cursor to resume from.
#[tokio::test]
async fn the_stream_deadline_ends_a_response_with_a_trailer() {
    let f = fixture_with(N, 16, generous_test_gate(), generous_bulk_gate(), |limits| {
        limits.stream_deadline_ms = 0;
    })
    .await;
    let token = token(&f.server, &["0"]).await;
    // The session's geometry is built by a viewport first, so the cancellation meets the read
    // and not that build. The same deadline cuts the viewport's own response after its first
    // flush, by which time the geometry is built, so how that response ends is not read.
    let _ = f
        .server
        .client
        .post(f.server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 0 }))
        .send()
        .await;
    let decoded = items_ok(
        &f.server,
        &token,
        &json!({ "view": "s0", "fields": ["score"], "page_rows": 10 }),
    )
    .await;
    assert_eq!(decoded.trailer["ended_by"], "deadline");
    assert!(decoded.trailer["next"].is_string());
}

/// **`/v1/meta` publishes each field's homes and the two page ceilings.**
#[tokio::test]
async fn meta_publishes_homes_and_the_page_ceilings() {
    let f = fixture_with(N, 16, generous_test_gate(), generous_bulk_gate(), |limits| {
        limits.max_page_rows = 1_234;
        limits.max_page_bytes = 56_789;
    })
    .await;
    let token = token(&f.server, &["0"]).await;
    let meta: Value = f
        .server
        .client
        .get(f.server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta["selection"]["max_page_rows"], 1_234);
    assert_eq!(meta["selection"]["max_page_bytes"], 56_789);
    let homes = |name: &str| -> Vec<String> {
        let scalar = meta["declared_scalars"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("{name} is declared"));
        serde_json::from_value(scalar["homes"].clone()).unwrap()
    };
    assert!(homes("score").contains(&"rendered".to_string()));
    assert!(homes("archive").contains(&"rendered".to_string()));
    assert!(homes("year").contains(&"value_column".to_string()));
    assert_eq!(homes("note"), vec!["record".to_string()]);

    // A page of a larger request is held to the published ceiling.
    let decoded = items_ok(&f.server, &token, &json!({ "view": "s0", "fields": [], "pages": 1 })).await;
    assert_eq!(decoded.head["page_rows"], 1_234);
    assert_eq!(decoded.pages[0].0.num_rows(), 1_234);
}
