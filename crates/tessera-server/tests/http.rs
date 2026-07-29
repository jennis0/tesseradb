//! Task 13, Step 1: the three HTTP planes, end to end, spawned in-process on port 0 against a
//! small synthetic bundle (the same fixture pattern Task 11's `tests/viewport.rs` uses).

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, BinaryArray, Float32Array, Float64Array, StringArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use parking_lot::Mutex;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, SessionRegistry};
use tessera_spatial::Extent;

const N_ITEMS: u64 = 1_000;
const SESSION_CREDENTIAL: &str = "session-secret";
const OPERATOR_CREDENTIAL: &str = "operator-secret";

fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![0, 1]
    } else {
        vec![0]
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    write_points(points_path);
    write_pairs(pairs_path);
    let args = BuildArgs {
        points: points_path.to_path_buf(),
        pairs: pairs_path.to_path_buf(),
        out: out.to_path_buf(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
    };
    build(&args).expect("fixture build should succeed");
}

/// `tessera_build`'s external-id convention (see its `write_external_ids` doc): the source
/// corpus's numeric id, 8 bytes little-endian.
fn external_id_of(source_id: u64) -> Vec<u8> {
    source_id.to_le_bytes().to_vec()
}

struct TestServer {
    viewer_addr: SocketAddr,
    session_addr: SocketAddr,
    control_addr: SocketAddr,
    client: reqwest::Client,
}

impl TestServer {
    fn viewer_url(&self, path: &str) -> String {
        format!("http://{}{}", self.viewer_addr, path)
    }
    fn session_url(&self, path: &str) -> String {
        format!("http://{}{}", self.session_addr, path)
    }
    fn control_url(&self, path: &str) -> String {
        format!("http://{}{}", self.control_addr, path)
    }
}

async fn spawn_server(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> TestServer {
    let engine = Engine::open(
        bundle_root,
        cache_dir,
        wal_path,
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
        },
    )
    .expect("engine should open against a freshly built bundle");

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        max_k: 200,
        min_visible_members: 10,
        session_credential: SESSION_CREDENTIAL.to_string(),
        operator_credential: OPERATOR_CREDENTIAL.to_string(),
    });

    let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let viewer_addr = viewer_listener.local_addr().unwrap();
    let session_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session_addr = session_listener.local_addr().unwrap();
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control_listener.local_addr().unwrap();

    let viewer_router = tessera_server::viewer::router(Arc::clone(&state));
    let session_router = tessera_server::session::router(Arc::clone(&state));
    let control_router = tessera_server::control::router(Arc::clone(&state));

    tokio::spawn(async move { axum::serve(viewer_listener, viewer_router).await });
    tokio::spawn(async move { axum::serve(session_listener, session_router).await });
    tokio::spawn(async move { axum::serve(control_listener, control_router).await });

    TestServer {
        viewer_addr,
        session_addr,
        control_addr,
        client: reqwest::Client::new(),
    }
}

async fn authorise(server: &TestServer, terms: &[&str]) -> serde_json::Value {
    let auth_data = serde_json::json!({ "terms": terms }).to_string();
    let encoded = base64::engine::general_purpose::STANDARD.encode(auth_data);
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "auth_data": encoded }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "authorise should succeed");
    resp.json().await.unwrap()
}

type TileRow = (u64, u64, u64);
type PointRow = (u32, f32, f32);

/// Decode the framed Arrow payload `tessera_wire::viewport_ipc` builds: a 4-byte LE length, the
/// tile stream, then the points stream. Returns `(tiles, points)` where each tile is
/// `(tile, visible, matched)` and each point is `(handle, x, y)`.
fn decode_viewport(bytes: &[u8]) -> (Vec<TileRow>, Vec<PointRow>) {
    let tile_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let tile_bytes = &bytes[4..4 + tile_len];
    let points_bytes = &bytes[4 + tile_len..];

    let mut tiles = Vec::new();
    let reader = StreamReader::try_new(Cursor::new(tile_bytes), None).unwrap();
    for batch in reader {
        let batch = batch.unwrap();
        let tile = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let visible = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let matched = batch
            .column(2)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            tiles.push((tile.value(i), visible.value(i), matched.value(i)));
        }
    }

    let mut points = Vec::new();
    let reader = StreamReader::try_new(Cursor::new(points_bytes), None).unwrap();
    for batch in reader {
        let batch = batch.unwrap();
        let handle = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        let x = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap();
        let y = batch
            .column(2)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            points.push((handle.value(i), x.value(i), y.value(i)));
        }
    }

    (tiles, points)
}

fn build_ingest_batch(rows: &[(u64, f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext: Vec<Vec<u8>> = rows
        .iter()
        .map(|(id, _, _, _)| external_id_of(*id))
        .collect();
    let ext_array = BinaryArray::from_iter_values(ext.iter().map(|v| v.as_slice()));
    let x_array = Float32Array::from_iter_values(rows.iter().map(|(_, x, _, _)| *x));
    let y_array = Float32Array::from_iter_values(rows.iter().map(|(_, _, y, _)| *y));
    let access_array = StringArray::from_iter_values(rows.iter().map(|(_, _, _, a)| *a));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ext_array),
            Arc::new(x_array),
            Arc::new(y_array),
            Arc::new(access_array),
        ],
    )
    .unwrap();

    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

#[tokio::test]
async fn a_authorise_then_viewport_succeeds_with_matching_counts() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("x-tessera-pin"));
    let bytes = resp.bytes().await.unwrap();
    let (tiles, points) = decode_viewport(&bytes);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].1, N_ITEMS, "every item carries term 0");
    assert_eq!(tiles[0].1, tiles[0].2, "matched == visible (no filters)");
    assert_eq!(points.len(), 5, "k=5 caps sampled points, not the count");
}

#[tokio::test]
async fn b_missing_or_garbage_token_is_401() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });

    let resp_missing = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_missing.status(), 401);

    let resp_garbage = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth("not-a-real-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_garbage.status(), 401);
}

#[tokio::test]
async fn c_revoke_then_viewport_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let token_id = auth["token_id"].as_u64().unwrap();

    let resp = server
        .client
        .post(server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "token_id": token_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 403 || resp.status() == 401,
        "revoked token must be rejected as 403 or 401, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn d_unknown_slice_404_and_malformed_bbox_422() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "does-not-exist", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unknown");

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [1000.0, 0.0, 0.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract");
}

#[tokio::test]
async fn e_suppress_via_changes_drops_the_count_without_reauthorising() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let viewport_req = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_before, _) = decode_viewport(&resp.bytes().await.unwrap());

    const SUPPRESS_SOURCE_ID: u64 = 5;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());

    assert_eq!(tiles_after[0].1, tiles_before[0].1 - 1);
}

#[tokio::test]
async fn f_ingest_is_wal_before_ack_and_idempotent() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch(&[(N_ITEMS + 1, 10.0, 10.0, "0")]);

    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 1);
    assert_eq!(json["over_bound"], 0);

    // Replay of the same batch id + body: idempotent 200.
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Same id, different body: 409.
    let different_body = build_ingest_batch(&[(N_ITEMS + 2, 11.0, 11.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(different_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn g_stale_pin_is_410() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "pin": { "prefix": "v00000", "segments_version": 999 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "pin-expired");
}

#[tokio::test]
async fn g2_pins_survive_overlay_swaps() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    let pin_header = resp
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let pin: serde_json::Value = serde_json::from_str(&pin_header).unwrap();
    let (tiles_before, _) = decode_viewport(&resp.bytes().await.unwrap());

    const SUPPRESS_SOURCE_ID: u64 = 7;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Re-query WITH the pin taken before the suppression: 200 (not 410 — a pin fixes geometry,
    // never authorisation), and the count reflects the suppression immediately.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "pin": pin
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a pin must survive an overlay swap");
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());
    assert_eq!(tiles_after[0].1, tiles_before[0].1 - 1);
}

#[tokio::test]
async fn h_config_missing_disclosure_refuses_to_start() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    std::env::set_var("TESSERA_TEST_H_SESSION", SESSION_CREDENTIAL);
    std::env::set_var("TESSERA_TEST_H_OPERATOR", OPERATOR_CREDENTIAL);

    let toml_text = format!(
        r#"
        [bundle]
        path = "{bundle}"
        cache = "{cache}"
        wal = "{wal}"
        [plugin]
        module = "builtin:passthrough"
        [serve]
        viewer = "127.0.0.1:0"
        session = "127.0.0.1:0"
        control = "127.0.0.1:0"
        session_credential_env = "TESSERA_TEST_H_SESSION"
        operator_credential_env = "TESSERA_TEST_H_OPERATOR"
        "#,
        bundle = bundle_root.display(),
        cache = tmp.path().join("cache").display(),
        wal = tmp.path().join("wal.log").display(),
    );
    let config_path = tmp.path().join("tessera.toml");
    std::fs::write(&config_path, toml_text).unwrap();

    let result = tessera_server::prepare(&config_path);
    assert!(
        result.is_err(),
        "a config missing [disclosure] must refuse to start"
    );
}
