//! Shared fixtures for `tessera-server`'s integration tests.
//!
//! Task 0c (Phase 2 stage 2.1) split the former single `tests/http.rs` into two binaries by
//! subject — `http.rs` (viewer plane, session plane, config, byte shape, the compute-admission
//! gate) and `http_write.rs` (the control plane's write path) — so that stage 2.1's parallel
//! tracks own disjoint files. This module holds every fixture both of them use; nothing here
//! changed in the split beyond gaining `pub`.

// Each integration-test binary compiles this module separately, so a fixture used by only one of
// them is genuinely dead code in the other. Allowing it here is what keeps the two halves from
// each carrying their own copy — which is the drift this module exists to prevent.
#![allow(dead_code)]

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, Float32Array, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use parking_lot::Mutex;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, ComputeGate, SessionRegistry};
use tessera_spatial::Extent;
use tessera_types::IdentityKey;

pub const N_ITEMS: u64 = 1_000;
pub const SESSION_CREDENTIAL: &str = "session-secret";

pub const OPERATOR_CREDENTIAL: &str = "operator-secret";
/// Fixed test key, matching `tessera-build`'s own test fixtures — not sensitive, this repository
/// contains no real deployment key.
pub const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// The fixture bundle's `identity.epoch` — contracts §2.2's transport-identity epoch, which
/// `/v1/meta` reports and `/v1/items` compares an optional `epoch` against.
pub const FIXTURE_EPOCH: u32 = 1;

pub fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

pub fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

pub fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![0, 1]
    } else {
        vec![0]
    }
}

/// Parameterised over the item count — see [`build_fixture_n`]'s doc for why (calibration task:
/// the byte-equality tests below need a regime that clears `SERIAL_FALLBACK_MAX_ROWS`, well above
/// this file's default `N_ITEMS`).
pub fn write_points_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
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

/// Parameterised over the item count — see [`build_fixture_n`]'s doc for why.
pub fn write_pairs_n(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..n {
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

/// See [`build_fixture`] — parameterised, same reason as [`write_points_n`].
pub fn build_fixture_n(out: &Path, points_path: &Path, pairs_path: &Path, n: u64) {
    write_points_n(points_path, n);
    write_pairs_n(pairs_path, n);
    let args = BuildArgs {
        points: points_path.to_path_buf(),
        pairs: pairs_path.to_path_buf(),
        out: out.to_path_buf(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        identity_epoch: FIXTURE_EPOCH,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
    };
    build(&args).expect("fixture build should succeed");
}

pub fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    build_fixture_n(out, points_path, pairs_path, N_ITEMS)
}

/// `tessera_build`'s external-id convention (see its `write_external_ids` doc): the source
/// corpus's numeric id, 8 bytes little-endian.
pub fn external_id_of(source_id: u64) -> Vec<u8> {
    source_id.to_le_bytes().to_vec()
}

pub struct TestServer {
    pub viewer_addr: SocketAddr,
    pub session_addr: SocketAddr,
    pub control_addr: SocketAddr,
    pub client: reqwest::Client,
}

impl TestServer {
    pub fn viewer_url(&self, path: &str) -> String {
        format!("http://{}{}", self.viewer_addr, path)
    }
    pub fn session_url(&self, path: &str) -> String {
        format!("http://{}{}", self.session_addr, path)
    }
    pub fn control_url(&self, path: &str) -> String {
        format!("http://{}{}", self.control_addr, path)
    }
}

/// The `EngineConfig` every test but the Task 3 concurrency tests uses.
pub fn default_engine_config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        // Saturate theta: these tests assert HTTP shape and masking, not density. See
        // tessera-engine's tests/viewport.rs `config()` for the full reasoning.
        theta_target_marks: u64::MAX,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: tessera_engine::default_compute_threads(),
        // The shipped defaults for lifecycle §2.2's two pin bounds — see the same two lines in
        // tessera-engine's `tests/common/mod.rs`. Inert until Task 4 (Task 0 gate, F3).
        pin_ttl_secs: 300,
        pins_per_session_max: 4,
    }
}

pub async fn spawn_server(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> TestServer {
    spawn_server_with_config(bundle_root, cache_dir, wal_path, default_engine_config()).await
}

/// Task 4's default test gate: generous enough that no test written before this task's admission
/// gate existed can ever observe it (every one of those tests issues at most a handful of
/// sequential requests) — only the gate-specific tests below construct a deliberately tiny
/// [`ComputeGate`] to exercise shedding.
pub fn generous_test_gate() -> ComputeGate {
    ComputeGate::new(64, 64, 250)
}

/// Like [`spawn_server`], but with a caller-supplied `EngineConfig` — Task 3's concurrency tests
/// need a much wider underlay budget than every other test in this file to engineer a
/// deterministic slow request (see `healthz_stays_prompt_while_a_long_viewport_runs`'s doc), and
/// duplicating the whole engine-open-plus-three-listeners dance per test would be worse than one
/// extra parameter.
pub async fn spawn_server_with_config(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    config: EngineConfig,
) -> TestServer {
    spawn_server_with_config_and_gate(
        bundle_root,
        cache_dir,
        wal_path,
        config,
        generous_test_gate(),
    )
    .await
}

/// Like [`spawn_server_with_config`], but also with a caller-supplied [`ComputeGate`] — Task 4's
/// admission-gate tests need a deliberately tiny gate (`compute_admission=1, compute_queue=0`) to
/// hold saturated deterministically, which every other test in this file must not be affected by.
pub async fn spawn_server_with_config_and_gate(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    config: EngineConfig,
    compute_gate: ComputeGate,
) -> TestServer {
    let max_k = config.max_k;
    let engine = Engine::open(bundle_root, cache_dir, wal_path, Passthrough::new(), config)
        .expect("engine should open against a freshly built bundle");

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        max_k,
        compute_gate,
        // On, so the header assertions below exercise the emission path rather than only its
        // absence. The compile-time `bench-timing` gate still decides whether anything is sent.
        stage_timing: true,
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

pub async fn authorise(server: &TestServer, terms: &[&str]) -> serde_json::Value {
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

/// `POST /v1/items/{tessera_id}` with no body fields set (no pin, no epoch).
pub async fn post_item(server: &TestServer, token: &str, tessera_id: u64) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
}

pub type TileRow = (u64, u64, u64);
pub type PointRow = (u64, f32, f32);

/// Decode the framed Arrow payload `tessera_wire::viewport_ipc` builds: a 4-byte LE length, the
/// tile stream, then the points stream. Returns `(tiles, points)` where each tile is
/// `(tile, visible, matched)` and each point is `(tessera_id, x, y)`.
pub fn decode_viewport(bytes: &[u8]) -> (Vec<TileRow>, Vec<PointRow>) {
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
        let tessera_id = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
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
            points.push((tessera_id.value(i), x.value(i), y.value(i)));
        }
    }

    (tiles, points)
}

/// `GET /control/status`'s body, for asserting `entity_id_high_water` is unchanged across a
/// rejected batch (contracts §3.1: a 409 batch has NO effect).
pub async fn control_status(server: &TestServer) -> serde_json::Value {
    server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
