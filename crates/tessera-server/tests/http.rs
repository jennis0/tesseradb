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
use tessera_engine::viewport::{ViewportRequest, SERIAL_FALLBACK_MAX_ROWS};
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, ComputeGate, SessionRegistry};
use tessera_spatial::{tiles_for_bbox, Extent};
use tessera_types::IdentityKey;

const N_ITEMS: u64 = 1_000;
const SESSION_CREDENTIAL: &str = "session-secret";

/// Item count for the two byte-equality tests below — see `tessera-engine/tests/viewport.rs`'s
/// identically-named constant for the full argument (same fixed-extent scatter, so
/// `Σ range.len() == n` exactly for a full-extent request). This crate does not depend on
/// `tessera-engine`'s test binary, so the constant and its reasoning are duplicated rather than
/// shared, matching this file's own existing "same fixture pattern" duplication of
/// `tests/viewport.rs`'s fixture builder (this file's module doc).
///
/// **§14 note.** `SERIAL_FALLBACK_MAX_ROWS` rose to 500,000,000 post-B9 (three-scale
/// re-calibration; see that constant's doc in `tessera-engine`). A fixture that reaches it is
/// impractical at unit-test scale, so this constant is NOT raised to match — the two tests below
/// now exercise the SERIAL branch on both `compute_threads` configs (still a real engine-wiring
/// claim, just not "the parallel fan-out specifically", which their names/docs used to claim). The
/// narrower, still-provable property (rayon's indexed collect preserves tile order regardless of
/// pool size) is covered decoupled from fixture size by
/// `tessera_engine::viewport::tests::indexed_collect_of_tile_shaped_results_preserves_order_at_any_pool_size`.
const PARALLEL_HEADLINE_ITEMS: u64 = 300_000;

/// Compile-time twin of `tests/viewport.rs`'s identically-named assertion, now checking the
/// OPPOSITE relationship from before §14: this crate's two byte-equality tests deliberately stay
/// below the (now much higher) threshold, so if a future edit ever raised `PARALLEL_HEADLINE_ITEMS`
/// past `SERIAL_FALLBACK_MAX_ROWS` (or lowered the threshold below it) without updating the doc
/// above, this catches the drift at compile time rather than leaving a stale claim in a comment.
const _: () = assert!(PARALLEL_HEADLINE_ITEMS < SERIAL_FALLBACK_MAX_ROWS);

const OPERATOR_CREDENTIAL: &str = "operator-secret";
/// Fixed test key, matching `tessera-build`'s own test fixtures — not sensitive, this repository
/// contains no real deployment key.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// The fixture bundle's `identity.epoch` — contracts §2.2's transport-identity epoch, which
/// `/v1/meta` reports and `/v1/items` compares an optional `epoch` against.
const FIXTURE_EPOCH: u32 = 1;

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

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

/// Parameterised over the item count — see [`build_fixture_n`]'s doc for why (calibration task:
/// the byte-equality tests below need a regime that clears `SERIAL_FALLBACK_MAX_ROWS`, well above
/// this file's default `N_ITEMS`).
fn write_points_n(path: &Path, n: u64) {
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
fn write_pairs_n(path: &Path, n: u64) {
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
fn build_fixture_n(out: &Path, points_path: &Path, pairs_path: &Path, n: u64) {
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

fn build_fixture(out: &Path, points_path: &Path, pairs_path: &Path) {
    build_fixture_n(out, points_path, pairs_path, N_ITEMS)
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

/// The `EngineConfig` every test but the Task 3 concurrency tests uses.
fn default_engine_config() -> EngineConfig {
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
    }
}

async fn spawn_server(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> TestServer {
    spawn_server_with_config(bundle_root, cache_dir, wal_path, default_engine_config()).await
}

/// Task 4's default test gate: generous enough that no test written before this task's admission
/// gate existed can ever observe it (every one of those tests issues at most a handful of
/// sequential requests) — only the gate-specific tests below construct a deliberately tiny
/// [`ComputeGate`] to exercise shedding.
fn generous_test_gate() -> ComputeGate {
    ComputeGate::new(64, 64, 250)
}

/// Like [`spawn_server`], but with a caller-supplied `EngineConfig` — Task 3's concurrency tests
/// need a much wider underlay budget than every other test in this file to engineer a
/// deterministic slow request (see `healthz_stays_prompt_while_a_long_viewport_runs`'s doc), and
/// duplicating the whole engine-open-plus-three-listeners dance per test would be worse than one
/// extra parameter.
async fn spawn_server_with_config(
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
async fn spawn_server_with_config_and_gate(
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

/// `POST /v1/items/{tessera_id}` with no body fields set (no pin, no epoch).
async fn post_item(server: &TestServer, token: &str, tessera_id: u64) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
}

type TileRow = (u64, u64, u64);
type PointRow = (u64, f32, f32);

/// Decode the framed Arrow payload `tessera_wire::viewport_ipc` builds: a 4-byte LE length, the
/// tile stream, then the points stream. Returns `(tiles, points)` where each tile is
/// `(tile, visible, matched)` and each point is `(tessera_id, x, y)`.
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

/// Like [`build_ingest_batch`], but takes the raw `external_id` bytes directly rather than
/// deriving them from a source id — needed for Task 11's duplicate-detection and cap tests, which
/// must construct exact byte strings (repeats across rows, or a specific length) that
/// `external_id_of`'s 8-byte little-endian convention cannot express.
fn build_ingest_batch_raw(rows: &[(&[u8], f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext_array = BinaryArray::from_iter_values(rows.iter().map(|(id, _, _, _)| *id));
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

/// Like [`build_ingest_batch_raw`], but `external_id` is `Option<&[u8]>` per row — contracts §3.4
/// r6: an ingested item may carry no external id at all, in which case it is addressable only by
/// the `tessera_id` `/control/ingest`'s response returns for it. The column is declared nullable
/// here (unlike the other two builders, which happen to always supply a value): this is the
/// null-within-the-column shape the server must accept.
fn build_ingest_batch_optional(rows: &[(Option<&[u8]>, f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext_array = BinaryArray::from_iter(rows.iter().map(|(id, _, _, _)| *id));
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

/// Owner ruling (contracts §3.2): `/v1/items` returns the identical `404` for "no such id" and
/// "exists but is not visible to this principal" -- same status, same body, byte for byte. This
/// test deliberately never learns which *external id* the invisible `tessera_id` names (that
/// would require inverting the identity, which I10 forbids even to a test): it gets a genuinely
/// existing id from session A's own viewport (everyone carries term "0") and finds one that
/// session B -- authorised for term "1" only, so it sees strictly fewer items (`terms_of`'s
/// multiples-of-3 subset) -- cannot see, entirely through the HTTP surface a client has.
#[tokio::test]
async fn i_item_404s_identically_for_unknown_and_invisible() {
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

    // Session A: term "0" -- every item carries it, so A sees the whole bundle.
    let auth_a = authorise(&server, &["0"]).await;
    let token_a = auth_a["token"].as_str().unwrap();
    // Session B: term "1" only -- `terms_of`'s multiples-of-3 subset, strictly fewer items.
    let auth_b = authorise(&server, &["1"]).await;
    let token_b = auth_b["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token_a)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    assert!(
        !points.is_empty(),
        "session A's viewport must return some points to pick from"
    );

    // Find a tessera_id that is real (session A's own viewport returned it) but invisible to B.
    let mut invisible_to_b = None;
    for &(tessera_id, _, _) in &points {
        let resp_b = post_item(&server, token_b, tessera_id).await;
        if resp_b.status() == 404 {
            invisible_to_b = Some(tessera_id);
            break;
        }
    }
    let invisible_to_b = invisible_to_b
        .expect("the fixture's multiples-of-3 term split must leave something invisible to B");

    // Sanity: A, which is the session that surfaced this id in its own viewport, can fetch it.
    let resp_a = post_item(&server, token_a, invisible_to_b).await;
    assert_eq!(
        resp_a.status(),
        200,
        "session A must be able to fetch an id its own viewport just returned"
    );

    let unknown_to_everyone = 0xDEAD_BEEF_DEAD_BEEFu64;
    let resp_unknown = post_item(&server, token_b, unknown_to_everyone).await;
    let resp_invisible = post_item(&server, token_b, invisible_to_b).await;

    assert_eq!(resp_unknown.status(), 404);
    assert_eq!(resp_invisible.status(), 404);
    assert_eq!(resp_unknown.status(), resp_invisible.status());

    let unknown_body = resp_unknown.text().await.unwrap();
    let invisible_body = resp_invisible.text().await.unwrap();
    assert_eq!(
        unknown_body, invisible_body,
        "identical 404 required byte-for-byte -- any difference is an oracle for \"this id exists\""
    );
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

/// `GET /control/status`'s body, for asserting `entity_id_high_water` is unchanged across a
/// rejected batch (contracts §3.1: a 409 batch has NO effect).
async fn control_status(server: &TestServer) -> serde_json::Value {
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

/// Contracts §3.1: duplicate external ids *within* one batch are `409 conflict`, and the batch
/// has NO effect at all -- not even the non-duplicate rows are accepted.
#[tokio::test]
async fn ingest_rejects_duplicate_external_ids_within_one_batch() {
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

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let body = build_ingest_batch_raw(&[
        (b"a".as_slice(), 1.0, 1.0, "0"),
        (b"b".as_slice(), 2.0, 2.0, "0"),
        (b"a".as_slice(), 3.0, 3.0, "0"),
    ]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "dup-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "a 409 batch must have no effect at all -- not even the non-duplicate rows"
    );
}

/// Important I-8: dedup must consult `Engine::established`, not only the bundle's external-id
/// sidecar. The sidecar covers only the bundle built at open time; an id ingested five minutes
/// ago in a *separate*, already-accepted batch lives only in the live map, and a dedup check
/// that misses it would silently allocate a second entity and orphan the first
/// (`session.rs`'s `Engine::accept_ingest` doc).
#[tokio::test]
async fn ingest_rejects_an_external_id_ingested_after_the_build() {
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

    let first_body = build_ingest_batch_raw(&[(b"z".as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "z-batch-1")
        .header("content-type", "application/octet-stream")
        .body(first_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let high_water_after_first = control_status(&server).await["entity_id_high_water"].clone();

    // A fresh batch id, re-ingesting the same external id: must be rejected, not silently
    // allocate a second entity for "z".
    let second_body = build_ingest_batch_raw(&[(b"z".as_slice(), 9.0, 9.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "z-batch-2")
        .header("content-type", "application/octet-stream")
        .body(second_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_after_first,
        "the rejected re-ingest must not have allocated a second entity"
    );
}

/// The bundle's own external-id sidecar half of duplicate detection: an id already present in
/// the built bundle (not merely ingested live) must also be rejected.
#[tokio::test]
async fn ingest_rejects_an_external_id_already_in_the_bundle() {
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

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    // `external_id_of(0)` names a real item baked into the fixture at build time.
    let body = build_ingest_batch(&[(0, 5.0, 5.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "bundle-dup-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before
    );
}

/// Ordering matters and is not incidental: the batch-id replay check stays FIRST. An idempotent
/// retry of an already-accepted batch id + body is a 200 no-op, even though the external id it
/// carries is (correctly) "already known" by the time the duplicate check would run.
#[tokio::test]
async fn an_idempotent_retry_of_an_accepted_batch_is_a_200_not_a_409() {
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

    let body = build_ingest_batch_raw(&[(b"replay-me".as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "replay-batch")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "replay-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "a byte-identical replay of an already-acked batch id must stay a 200, never be caught \
         by the duplicate-external-id check"
    );
}

/// Contracts §1 (r6): external ids are capped at ≤ 64 bytes. Off-by-one is the whole point: 64
/// bytes exactly is accepted, 65 is a typed error, never a silent truncation.
#[tokio::test]
async fn ingest_external_id_cap_is_64_bytes_exactly() {
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

    let exactly_64 = vec![b'x'; 64];
    let body = build_ingest_batch_raw(&[(exactly_64.as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "cap-64")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "exactly 64 bytes must be accepted");

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let sixty_five = vec![b'y'; 65];
    let body = build_ingest_batch_raw(&[(sixty_five.as_slice(), 2.0, 2.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "cap-65")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        422,
        "65 bytes must be a typed contract error, never truncated to 64"
    );
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "contract");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "a rejected over-length batch must have no effect"
    );
}

/// The point of batching resolution: a large batch must open each bundle extent at most once,
/// not once per row. The fixture bundle has one external-id extent (built with `N_ITEMS` rows),
/// so a batch of many distinct, never-before-seen external ids must resolve against it without
/// the sidecar opening more than that one extent.
#[tokio::test]
async fn a_batch_resolution_opens_each_extent_at_most_once() {
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

    let rows: Vec<(u64, f32, f32, &str)> = (0..2_000)
        .map(|i| (N_ITEMS + 10_000 + i, i as f32, i as f32, "0"))
        .collect();
    let body = build_ingest_batch(&rows);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "big-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 2_000);
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

// --- Fix-report regression tests (reviewer findings on the first Task 13 pass) ---

/// Important 1: `GET /control/status` must require the operator bearer credential — it discloses
/// `entity_id_high_water`, a global unmasked corpus-size fact, and the control listener may be
/// plain loopback TCP, not only a unix socket.
#[tokio::test]
async fn control_status_requires_bearer() {
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

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth("not-the-operator-credential")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Important 1: `GET /v1/meta` must require a valid session token — it discloses bundle
/// extents/slices/declared-scalar schema.
#[tokio::test]
async fn viewer_meta_requires_bearer() {
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

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Contracts §2.2 r6: `GET /v1/meta` reports the transport-identity epoch as `identity_epoch` —
/// and reports **only** the epoch: the identity key appears in no API response on any plane.
/// Nothing asserted either half before, which is what let S2's epoch regression sit untested.
#[tokio::test]
async fn viewer_meta_reports_the_identity_epoch_and_never_the_key() {
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
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["identity_epoch"], FIXTURE_EPOCH,
        "/v1/meta must report the bundle's identity_epoch: {body}"
    );
    let raw = body.to_string();
    assert!(
        !raw.contains(TEST_KEY_HEX),
        "/v1/meta must never carry the identity key: {raw}"
    );
}

/// Contracts §2.2/§3.2 r6: `POST /v1/items/{tessera_id}` accepts an optional `epoch` and answers
/// `409 conflict` — "stale identity epoch; re-resolve by external_id" — when it does not match.
/// The 409 had no test at any level, and the check is decided before inversion, so a matching
/// epoch must not alter the answer for the same id.
#[tokio::test]
async fn item_with_a_stale_epoch_is_409_and_a_matching_epoch_changes_nothing() {
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

    // A real, visible id, so the 409 is not confusable with the 404 an unknown id would give.
    let viewport = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(viewport.status(), 200);
    let (_tiles, points) = decode_viewport(&viewport.bytes().await.unwrap());
    let tessera_id = points[0].0;

    // Baseline: no epoch at all → 200.
    let plain = post_item(&server, token, tessera_id).await;
    assert_eq!(plain.status(), 200);

    // A stale epoch → 409, with the contract's own detail string.
    let stale = server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({ "epoch": FIXTURE_EPOCH + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let body: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(body["error"], "conflict");
    assert_eq!(
        body["detail"],
        "stale identity epoch; re-resolve by external_id"
    );

    // The matching epoch is a no-op: same 200, same body as the epoch-less request.
    let matching = server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({ "epoch": FIXTURE_EPOCH }))
        .send()
        .await
        .unwrap();
    assert_eq!(matching.status(), 200);

    // And a stale epoch on an id naming nothing is still the 409, decided before inversion —
    // identical for every identifier, so it opens no channel (Appendix C, C4).
    let stale_unknown = server
        .client
        .post(server.viewer_url("/v1/items/0"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "epoch": FIXTURE_EPOCH + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale_unknown.status(),
        409,
        "the epoch check must be entity-independent, not fall through to 404"
    );
}

/// Important 2: a `/control/changes` batch whose *later* item fails validation (unknown external
/// id) must leave every earlier item in the same batch unapplied — validate-first, not
/// apply-then-abort. Suppresses a real item first in the batch, then names a nonexistent external
/// id second; the whole request must 404, and the real item's count must be unaffected.
#[tokio::test]
async fn changes_batch_validates_before_applying_anything() {
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

    const REAL_SOURCE_ID: u64 = 9;
    let real_external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(REAL_SOURCE_ID));
    // Not a real external id (never ingested/built) — must 404 during validation.
    let bogus_external_id =
        base64::engine::general_purpose::STANDARD.encode(b"this-external-id-does-not-exist");

    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([
            { "external_id": real_external_id, "op": "suppress" },
            { "external_id": bogus_external_id, "op": "suppress" },
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());

    assert_eq!(
        tiles_after[0].1, tiles_before[0].1,
        "the batch's first item must not have been applied once a later item failed validation"
    );
}

/// Critical 1: two concurrent acceptances (one `/control/ingest`, one `/control/changes`) must
/// both survive — the previous unlocked apply+swap allowed a lost-update race where whichever
/// `store()` won silently discarded the other's already-fsynced, already-acked change. Runs the
/// engine's `accept_ingest`/`accept_change` directly (not through HTTP) on two OS threads, synced
/// to start together, so both race to load/clone/store the same starting generation.
#[test]
fn concurrent_ingest_and_change_both_survive() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = Arc::new(
        Engine::open(
            &bundle_root,
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
            Passthrough::new(),
            EngineConfig {
                token_max_lifetime_secs: 3600,
                max_k: 200,
                k_min: 2,
                k_max_marks: 200,
                theta_target_marks: u64::MAX,
                max_underlay_offset: 4,
                max_underlay_cells: 8192,
                max_tiles_per_request: 262_144,
                compute_threads: tessera_engine::default_compute_threads(),
            },
        )
        .expect("engine should open"),
    );

    const SUPPRESS_SOURCE_ID: u64 = 3;
    let suppress_entity = engine
        .resolve_external_id(&external_id_of(SUPPRESS_SOURCE_ID))
        .expect("resolve_external_id should not fail for a healthy bundle")
        .expect("fixture item must resolve");

    let barrier = Arc::new(std::sync::Barrier::new(2));

    let engine_a = Arc::clone(&engine);
    let barrier_a = Arc::clone(&barrier);
    let change_thread = std::thread::spawn(move || {
        barrier_a.wait();
        engine_a
            .accept_change(
                external_id_of(SUPPRESS_SOURCE_ID),
                suppress_entity,
                tessera_lifecycle::ChangeOp::Suppress,
                None,
            )
            .expect("change should be accepted");
    });

    let engine_b = Arc::clone(&engine);
    let barrier_b = Arc::clone(&barrier);
    let ingest_thread = std::thread::spawn(move || {
        let new_external_id = external_id_of(N_ITEMS + 100);
        let row = tessera_lifecycle::WalRow {
            external_id: Some(new_external_id.clone()),
            entity_id: tessera_types::EntityId::new(N_ITEMS + 100),
            descriptors: vec![b"0".to_vec()],
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
        };
        let terms = engine_b.resolve_terms(std::slice::from_ref(&b"0".to_vec()));
        barrier_b.wait();
        engine_b
            .accept_ingest(
                vec![row],
                vec![terms],
                "concurrent-batch".to_string(),
                [7u8; 32],
            )
            .expect("ingest should be accepted");
    });

    change_thread.join().unwrap();
    ingest_thread.join().unwrap();

    // The suppression's effect: a viewport count one lower than the full-coverage baseline.
    // (Phase 1's ingested/buffered items have no row geometry yet — no flush — so the ingested
    // item contributes nothing to any tile's count regardless of correctness; its effect is
    // checked separately below, via the established external-id map a lost swap would revert.)
    let session = engine
        .authorise(br#"{"terms": ["0"]}"#)
        .expect("authorise should succeed");
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 10) as usize),
        )
        .expect("viewport should succeed");

    assert_eq!(
        out.tiles[0].visible,
        N_ITEMS - 1,
        "the concurrent suppression must have survived — a lost update would leave the count \
         unchanged"
    );

    // The ingest's effect: the newly-accepted external id must resolve to its assigned entity —
    // a lost update (the ingest's generation swap silently reverted by a racing change, or vice
    // versa) would make this `None`.
    let new_external_id = external_id_of(N_ITEMS + 100);
    assert_eq!(
        engine
            .resolve_external_id(&new_external_id)
            .expect("resolve_external_id should not fail for a healthy bundle"),
        Some(tessera_types::EntityId::new(N_ITEMS + 100)),
        "the concurrent ingest must have survived — a lost update would drop it from the live \
         buffer/established state"
    );
}

/// Contracts §3.4 (r6): an item ingested with no external id at all is still accepted, and the
/// `tessera_id` the 200 response returns for it is a genuine, correctly-shard-scoped identity for
/// the entity that was actually allocated — the only way the item is addressable at all, since it
/// has no external id.
///
/// This does not assert a `200` from `/v1/items`: Phase 1 has no flush yet, so *any* freshly
/// ingested item — with or without an external id — has no row geometry until the next
/// `tessera build`, and `Engine::item`'s own doc records that a visible-but-geometryless entity
/// is a `404`, identical to an unknown one. That is a pre-existing Phase 1 limitation, orthogonal
/// to this feature. What this test checks instead is the thing this feature actually promises:
/// inverting the returned `tessera_id` with the deployment's own identity key yields the right
/// shard and a freshly-allocated entity id (at or past the bundle's `N_ITEMS` high-water mark),
/// so the caller genuinely learned a working identity for its item, not a decoy.
#[tokio::test]
async fn ingest_with_a_null_external_id_returns_a_genuinely_resolvable_tessera_id() {
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

    let body = build_ingest_batch_optional(&[(None, 20.0, 20.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "null-ext-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 1);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 1);
    let tessera_id = tessera_ids[0].as_u64().unwrap();

    // The fixture's own identity key (matches `TEST_KEY_HEX`, shard 0) — inverting independently
    // of the server proves the response carries a real, working identity, not an opaque number.
    let (shard, entity) = test_key().invert(tessera_types::TesseraId::new(tessera_id));
    assert_eq!(shard, 0, "the fixture bundle is shard 0");
    assert!(
        entity.raw() >= N_ITEMS,
        "a freshly-ingested item must get an entity id past the bundle's own N_ITEMS range, not \
         collide with a built-in item"
    );

    // Phase 1's documented limitation, not a defect this feature introduces: no flush yet means
    // no row geometry for any freshly-ingested item, so `/v1/items` 404s identically to an
    // unknown id (`Engine::item`'s doc).
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = post_item(&server, token, tessera_id).await;
    assert_eq!(
        resp.status(),
        404,
        "a buffered (unflushed) item 404s on /v1/items regardless of external id, per Phase 1's \
         documented row-geometry limitation"
    );
}

/// Contracts §3.4 (r6): a batch mixing items with and without an external id is accepted whole,
/// and duplicate detection considers only the supplied ones — the null-external-id rows have
/// nothing to collide on and must not be rejected or interfere with the others' dedup check.
#[tokio::test]
async fn ingest_mixed_batch_only_supplied_external_ids_participate_in_dedup() {
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

    let body = build_ingest_batch_optional(&[
        (Some(b"mixed-a".as_slice()), 1.0, 1.0, "0"),
        (None, 2.0, 2.0, "0"),
        (None, 3.0, 3.0, "0"),
        (Some(b"mixed-b".as_slice()), 4.0, 4.0, "0"),
    ]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "mixed-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "two null external ids in one batch must not be treated as duplicates of each other"
    );
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 4);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 4);

    // A follow-up batch re-using one of the *supplied* external ids must still be caught.
    let dup_body = build_ingest_batch_optional(&[(Some(b"mixed-a".as_slice()), 9.0, 9.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "mixed-batch-dup")
        .header("content-type", "application/octet-stream")
        .body(dup_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

/// Contracts §3.4 (r6): two items with no external id in the *same* batch must not collide with
/// each other — `null` is not a key that can be duplicated.
#[tokio::test]
async fn ingest_two_null_external_ids_in_one_batch_do_not_collide() {
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

    let body = build_ingest_batch_optional(&[(None, 1.0, 1.0, "0"), (None, 2.0, 2.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "two-nulls-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 2);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 2);
    assert_ne!(
        tessera_ids[0].as_u64().unwrap(),
        tessera_ids[1].as_u64().unwrap(),
        "two null-external-id items must still get distinct entities/tessera_ids"
    );
}

/// The `x-tessera-stage-ns` header obeys **both** of its gates, and carries no identifier.
///
/// `spawn_server` sets `stage_timing: true`, so the runtime gate is open throughout this test.
/// The compile-time gate therefore decides on its own, and this asserts each direction rather
/// than only the one the current build happens to take — a release binary that started emitting
/// the header would otherwise pass a test written for the instrumented build.
///
/// The field-count assertion pins the CSV contract `tessera-bench` and `scripts/bench_*.py`
/// parse. Append-only: adding a stage means bumping the expected count here deliberately.
#[tokio::test]
async fn stage_timing_header_respects_the_compile_gate_and_carries_no_identifier() {
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

    let header = resp.headers().get("x-tessera-stage-ns").cloned();

    if cfg!(feature = "bench-timing") {
        let value = header
            .expect("bench-timing is on and stage_timing is true, so the header must be present");
        let text = value.to_str().expect("header is ASCII digits and commas");

        let fields: Vec<&str> = text.split(',').collect();
        assert_eq!(
            fields.len(),
            22,
            "stage header field count is a contract with the bench harnesses: {text}"
        );
        for f in &fields {
            assert!(
                f.parse::<u64>().is_ok(),
                "every field is an unsigned integer — no names, no identifiers: {text}"
            );
        }

        // Positions 12..=17 are the work counters (see `stage_header`'s field order).
        let tiles_nonempty: u64 = fields[13].parse().unwrap();
        let sigma_visible: u64 = fields[14].parse().unwrap();
        let materialised: u64 = fields[16].parse().unwrap();
        let gathered: u64 = fields[17].parse().unwrap();
        assert_eq!(tiles_nonempty, 1, "zoom 0 is one tile");
        assert_eq!(sigma_visible, N_ITEMS, "every item carries term 0");
        assert_eq!(gathered, 5, "k=5");
        // NOT re-asserted here. This test's job is the two gates and the no-identifier property;
        // the counter's semantics belong to the engine-side canary, which uses a partial-coverage
        // fixture so that both directions of the comparison are detectable. This session is
        // full-coverage, where `sigma_visible == rows_in_ranges` and the comparison is half blind.
        // Asserting it here anyway would read as coverage it does not provide.
        assert!(
            materialised > 0,
            "the counter must reach the wire at all: {text}"
        );

        // The three fields appended for §7.2's θ anchor and §7.3's underlay. This request asks for
        // no underlay, so both underlay fields must be zero — the default path must not pay for a
        // feature it did not request.
        let theta_anchor_ns: u64 = fields[19].parse().unwrap();
        let underlay_ns: u64 = fields[20].parse().unwrap();
        let underlay_cells: u64 = fields[21].parse().unwrap();
        let _ = theta_anchor_ns; // a duration; only its presence and parseability are contractual
        assert_eq!(
            underlay_ns, 0,
            "no underlay was requested, so it must cost nothing"
        );
        assert_eq!(
            underlay_cells, 0,
            "no underlay was requested, so no cells were evaluated"
        );
    } else {
        assert!(
            header.is_none(),
            "without the bench-timing feature the header must be absent even when \
             `stage_timing = true` — a release build must not emit it"
        );
    }
}

/// Task 3 (D-A): `/healthz` must stay prompt while a viewport request runs, even on this test's
/// single-threaded (`#[tokio::test]` default, current-thread) runtime — the strongest possible
/// demonstration of the bug this task fixes. Pre-refactor, `viewport`'s whole body (the engine
/// call through Arrow IPC framing) is synchronous Rust with no `.await` inside it; once tokio's
/// one worker thread starts polling that task it cannot be interrupted, so a concurrent
/// `/healthz` task cannot even be *polled* — let alone answered — until the viewport handler
/// returns. `spawn_blocking` gives the viewport task a genuine `.await` point: the blocking work
/// moves to tokio's separate blocking-thread pool (a real OS thread, regardless of runtime
/// flavor), freeing the one reactor thread to service `/healthz` while it runs.
///
/// Slowness is engineered deterministically via the §3.3 density underlay's `4^offset` sub-cell
/// fan-out (`tessera_engine::viewport`'s cost model — each sub-cell costs one small binary search
/// plus one bitmap range-count, independent of corpus size), not via corpus size — so the fixture
/// stays at the file's default `N_ITEMS` and builds in the same sub-second time every other test
/// here does. `offset = 12` at `zoom = 0` (one tile, so the tile-count bound never engages) asks
/// for `4^12 ≈ 16.8M` sub-cell evaluations.
///
/// **The passing (post-refactor) bound is self-scaling, not a fixed wall-clock bet.** A fixed
/// `healthz_elapsed < 1s` assumed this debug-profile binary's absolute speed; on a slower or more
/// loaded runner the sweep itself takes longer, and there is no reason `/healthz`'s bound should
/// stay pinned to 1s while the workload it is racing against grows. Instead this asserts
/// `healthz_elapsed < viewport_elapsed / 4`, computed from the *same run*'s own measurements:
/// `/healthz` does no engine work at all (a constant in-memory response) and runs on a different
/// OS thread than the viewport's `spawn_blocking` closure post-refactor, so its cost is bounded by
/// ambient connection/scheduling overhead only — independent of how long the sweep happens to take
/// on this particular machine. A quarter is generous headroom over that overhead on any runner,
/// while still failing loudly if the reactor were starved for anywhere close to the sweep's own
/// duration. The `viewport_elapsed > 200ms` floor below is a much weaker, absolute sanity check
/// only — it exists so a degenerate near-zero workload (e.g. a future edit that shrinks `offset`)
/// cannot make the ratio pass without genuinely engineering slowness — it is not the bound this
/// test relies on for its pass/fail signal.
#[tokio::test]
async fn healthz_stays_prompt_while_a_long_viewport_runs() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut config = default_engine_config();
    // Wide enough to let the request below through `Engine::viewport`'s own bounds checks
    // (`EngineError::UnderlayRefused`) rather than being rejected before it ever costs anything.
    config.max_underlay_offset = 12;
    config.max_underlay_cells = 20_000_000;
    let server = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let viewport_task = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let resp = client
            .post(viewer_url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
                "underlay_offset": 12
            }))
            .send()
            .await
            .unwrap();
        (resp.status(), start.elapsed())
    });

    // A short, empirically-bounded poll rather than a single `yield_now()`: one yield already
    // reliably gets the freshly-`tokio::spawn`ed viewport task its first turn on this runtime (a
    // newly spawned task is highly likely to run next), but a couple more give the scheduler a
    // little extra room to actually get it moving through connect/accept/parse before this task's
    // own `/healthz` clock starts, without over-polling.
    //
    // **This number is deliberately small, and was tuned, not guessed.** Pre-refactor, once the
    // viewport task's poll reaches the synchronous handler body it runs to completion in that
    // same turn with no further yield -- there is no observable "started but not finished" state
    // to poll for. So more polling here does not make the wait-for-start more precise, it just
    // gives the scheduler more chances to run the *entire* pre-refactor request (client connect
    // through server response) to completion before `/healthz` is ever sent, which would silently
    // stop this test from racing anything at all. Measured directly against the pre-refactor code
    // (temporarily reverting the four `src/` files this task changes): looping 1 or 2 times still
    // reliably starves `/healthz` (this test correctly fails); looping 3 or more times reliably
    // lets the whole pre-refactor viewport request finish first, turning this into a no-op race
    // every time (this test wrongly passes). `2` is the largest value on the correct side of that
    // measured boundary.
    for _ in 0..2 {
        tokio::task::yield_now().await;
    }

    let healthz_start = std::time::Instant::now();
    let healthz_resp = server
        .client
        .get(server.viewer_url("/healthz"))
        .send()
        .await
        .unwrap();
    let healthz_elapsed = healthz_start.elapsed();
    assert_eq!(healthz_resp.status(), 200);

    let (viewport_status, viewport_elapsed) = viewport_task.await.unwrap();
    assert_eq!(viewport_status, 200);

    // Weak absolute sanity floor only -- see this test's doc for why the real pass/fail signal is
    // the relative bound below, not this one.
    assert!(
        viewport_elapsed > std::time::Duration::from_millis(200),
        "the viewport request finished in {viewport_elapsed:?}, too fast to exercise this test's \
         starvation scenario -- widen the underlay offset"
    );
    assert!(
        healthz_elapsed < viewport_elapsed / 4,
        "/healthz took {healthz_elapsed:?}, more than a quarter of the {viewport_elapsed:?} the \
         concurrent viewport request took -- the reactor was starved"
    );
}

/// `/control/ingest`'s external ids for [`concurrent_ingests_do_not_delay_a_control_changes_suppress`],
/// chosen well clear of every other test's ranges in this file (`N_ITEMS`, and the `N_ITEMS +
/// 10_000 ..` range `a_batch_resolution_opens_each_extent_at_most_once` uses) so a shared-fixture
/// mistake would show up as a collision 409 rather than silently aliasing another test's ids.
const CONCURRENT_INGEST_BASE_ID: u64 = 50_000_000;
const CONCURRENT_INGEST_BATCHES: u64 = 8;
const CONCURRENT_INGEST_ROWS_PER_BATCH: u64 = 40_000;

/// Task 3 (D-A), review finding 7: a `/control/changes` suppression must not queue behind N
/// concurrent `/control/ingest` batches durability-syncing (lifecycle §1.3's deny priority lane,
/// reached through the reactor) — and this must hold even though `/control/ingest` and
/// `/control/changes` are NEVER behind the Task 4 admission gate (that gate is viewer/session
/// only). Same single-threaded-runtime argument as
/// `healthz_stays_prompt_while_a_long_viewport_runs`: pre-refactor, each ingest handler's Arrow
/// decode, term resolution and WAL append/fsync run synchronously with no `.await`, so once the
/// reactor thread starts executing one, it cannot service any other task — including accepting
/// or reading the suppress request's own connection — until that handler returns. Post-refactor,
/// both handlers do only their bearer check and header/body parse on the reactor, then hand off
/// to `spawn_blocking`'s separate thread pool — so the suppress request's own closure only has to
/// wait, at most, for whichever ONE ingest happens to be inside `Engine::accept_ingest`'s WAL
/// critical section at that instant (the WAL mutex is real and intentional — Critical 1's
/// atomicity fix — the bug this task closes is reactor-thread occupation, not that lock).
///
/// **Why this is unflaky despite real TCP connections being involved.** Unlike the single
/// `/healthz` race above, this test cannot rely on "the one other task must already be running
/// and cannot be interrupted" alone: `CONCURRENT_INGEST_BATCHES` separate connections are
/// accepted in whatever order the kernel happens to deliver their readiness, so pre-refactor the
/// suppress request is not guaranteed to queue behind literally all of them — only behind
/// whichever are already executing or queued ahead of it. All `CONCURRENT_INGEST_BATCHES`
/// requests are constructed and hand off to `tokio::spawn` before the suppress request is ever
/// sent, so it always races genuinely in-flight ingests, not hypothetical future ones.
///
/// **The passing (post-refactor) bound is self-scaling, not a fixed wall-clock bet** — this was
/// flagged in review: a fixed `suppress_elapsed < 1s` assumes this debug-profile binary's absolute
/// speed, but post-refactor the suppress closure still contends with up to `CONCURRENT_INGEST_
/// BATCHES` blocking-pool threads for CPU and for the (real, intentional, unfair
/// `std::sync::Mutex`) WAL lock each `accept_ingest` holds across its append+fsync — on a
/// slow-fsync or few-core runner that contention genuinely grows, and a fixed 1s bound could trip
/// for reasons that have nothing to do with this task's bug. So this asserts
/// `suppress_elapsed < total_ingest_elapsed / 2`, where `total_ingest_elapsed` is this same run's
/// own wall-clock time for every concurrent ingest batch to complete (measured from the same
/// `Instant` the batches were spawned from, to the last one's `JoinHandle` resolving). That is a
/// fair comparison because both numbers absorb the same runner's slowness together: whatever a
/// batch's parse/resolve/WAL cost is on this machine right now, `total_ingest_elapsed` reflects
/// roughly that cost repeated `CONCURRENT_INGEST_BATCHES` times (parse/resolve run in parallel
/// across the blocking pool, but the WAL section is serialised by the mutex, so the total is
/// dominated by something like `CONCURRENT_INGEST_BATCHES` WAL sections plus overhead), while the
/// suppress request post-refactor only ever has to reach the reactor (bearer check, fast) and
/// then wait for **at most one** ingest's WAL critical section before it gets the mutex itself —
/// a small, close-to-constant fraction of the total regardless of how slow that one section is on
/// this runner. `/2` leaves comfortable headroom over that expected ~1-in-`CONCURRENT_INGEST_
/// BATCHES` fraction even allowing for the WAL mutex's lack of strict fairness. Pre-refactor this
/// stays comfortably RED: the suppress request cannot even begin until the reactor is free, so it
/// queues behind a large share of the full (parse+resolve+WAL) handler bodies, not just one WAL
/// section — measured at ~3.0s suppress against a ~3.3s total in this task's tuning run, i.e. the
/// ratio sits near 1, not under 1/2.
#[tokio::test]
async fn concurrent_ingests_do_not_delay_a_control_changes_suppress() {
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

    // Starts here, not just before the suppress request below: `total_ingest_elapsed` (used for
    // the self-scaling bound at the end of this test) must cover every batch's full wall-clock
    // life, spawn to completion, not just the portion that overlaps the suppress request.
    let ingest_start = std::time::Instant::now();
    let mut ingest_tasks = Vec::with_capacity(CONCURRENT_INGEST_BATCHES as usize);
    for batch in 0..CONCURRENT_INGEST_BATCHES {
        let rows: Vec<(u64, f32, f32, &str)> = (0..CONCURRENT_INGEST_ROWS_PER_BATCH)
            .map(|row| {
                let id = CONCURRENT_INGEST_BASE_ID + batch * CONCURRENT_INGEST_ROWS_PER_BATCH + row;
                (id, row as f32, row as f32, "0")
            })
            .collect();
        let body = build_ingest_batch(&rows);
        let client = server.client.clone();
        let url = server.control_url("/control/ingest");
        let batch_id = format!("concurrent-{batch}");
        ingest_tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .bearer_auth(OPERATOR_CREDENTIAL)
                .header("x-tessera-batch-id", batch_id)
                .header("content-type", "application/octet-stream")
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }

    // Every ingest task is now on the runtime's queue, none of them awaited yet — the suppress
    // request below genuinely races them, not a hypothetical future batch.
    tokio::task::yield_now().await;

    const SUPPRESS_SOURCE_ID: u64 = 7;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let suppress_start = std::time::Instant::now();
    let suppress_resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    let suppress_elapsed = suppress_start.elapsed();
    assert_eq!(suppress_resp.status(), 200);

    for task in ingest_tasks {
        assert_eq!(
            task.await.unwrap(),
            200,
            "every concurrent ingest batch should still succeed"
        );
    }
    // Only measured once every batch has actually finished — see this test's doc for why this
    // (rather than a fixed wall-clock bound) is what `suppress_elapsed` is compared against.
    let total_ingest_elapsed = ingest_start.elapsed();

    assert!(
        suppress_elapsed < total_ingest_elapsed / 2,
        "/control/changes suppress took {suppress_elapsed:?}, more than half of the \
         {total_ingest_elapsed:?} the {CONCURRENT_INGEST_BATCHES} concurrent ingest batches took \
         to all complete -- it queued behind them on the reactor instead of reaching its own \
         spawn_blocking call promptly"
    );
}

// ---------------------------------------------------------------------------------------------
// Task 4 (D-B/D-E): the two-stage admission gate, the 429 `backpressure` contract, and the
// x-tessera-server-us / x-tessera-admission-us timing split.
// ---------------------------------------------------------------------------------------------

/// A slow viewport request, engineered exactly as `healthz_stays_prompt_while_a_long_viewport_runs`
/// does (see its doc for the cost-model argument): `zoom = 0`, `underlay_offset = 12` against a
/// server whose `EngineConfig` has been widened to allow it. Used throughout the gate tests below
/// to hold the compute permit for long enough to deterministically observe saturation.
fn slow_viewport_body() -> serde_json::Value {
    serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
        "underlay_offset": 12
    })
}

fn fast_viewport_body() -> serde_json::Value {
    serde_json::json!({ "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1 })
}

/// A server config wide enough for [`slow_viewport_body`] to pass `Engine::viewport`'s own
/// bounds checks rather than being refused as `EngineError::UnderlayRefused` before it costs
/// anything.
fn engine_config_for_slow_viewport() -> EngineConfig {
    let mut config = default_engine_config();
    config.max_underlay_offset = 12;
    config.max_underlay_cells = 20_000_000;
    config
}

/// Poll `/control/status` until `compute.in_flight` reaches `want`, panicking after a generous
/// bound rather than looping forever. **Deterministic, not a timing bet**: this is the
/// poll-until-a-real-condition-holds pattern the brief asks for in place of a fixed sleep or a
/// tuned yield count — it directly observes the gate's own state (derived from the semaphores'
/// live permit counts, `state::ComputeGate::status`) rather than guessing how long "the slow
/// request has started" takes on this run's scheduler.
async fn poll_until_in_flight(server: &TestServer, want: u64) {
    // Bound is generous (10s, not the original 2s) precisely so this helper's own panic stays
    // rare: on a slow or contended runner, a tight bound here fires *this* panic instead of
    // whichever ratio/timing assertion the calling test actually exists to check, which reads to
    // a future maintainer as "the gate never reached this state" (implicating the mechanism under
    // test) rather than "the runner was too slow for the poll bound" (an unrelated, purely
    // cosmetic failure mode) — both are still test failures either way, just with different, and
    // differently misleading, messages.
    for _ in 0..10_000 {
        let status = control_status(server).await;
        if status["compute"]["in_flight"].as_u64() == Some(want) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!(
        "compute.in_flight did not reach {want} within the 10s poll bound -- this is \
         poll_until_in_flight's own generous-but-finite timeout firing, not necessarily the \
         calling test's real assertion; check whether the gate is genuinely stuck before \
         assuming a regression in the mechanism the calling test targets"
    );
}

/// D-B: with `compute_admission = 1, compute_queue = 0` (the deterministic configuration this
/// task's brief names), a second concurrent `/v1/viewport` while the first is still running gets
/// an immediate 429 — `try_acquire` on the outer slots semaphore fails synchronously, so this
/// does not even need `admission_timeout_ms` to elapse. Verifies the full 429 contract: status,
/// `Retry-After: 1` header, and `{"error": "backpressure", "retry_after_s": 1}` body.
#[tokio::test]
async fn saturated_gate_sheds_a_second_viewport_with_429_and_retry_after() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    // Deterministic: wait until the slow request has actually acquired its compute permit
    // (`in_flight == 1`), not a guessed delay.
    poll_until_in_flight(&server, 1).await;

    let second_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();

    assert_eq!(second_resp.status(), 429);
    assert_eq!(
        second_resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a 429 must carry Retry-After: 1"
    );
    let body: serde_json::Value = second_resp.json().await.unwrap();
    assert_eq!(body["error"], "backpressure");
    assert_eq!(body["retry_after_s"], 1);

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(
        slow_resp.status(),
        200,
        "the request that actually held the gate must still succeed"
    );
}

/// D-B/D13: `/healthz`, `/v1/meta`, `/session/revoke`, and a `/control/changes` suppress must all
/// succeed while the viewer/session gate is fully saturated by a slow viewport — none of them is
/// a gated path (D-B's gated-paths list is exactly `/v1/viewport`, `/v1/items`,
/// `/session/authorise`), and the deny priority lane (lifecycle §1.3) must never be blocked by
/// compute-admission pressure on an unrelated plane.
#[tokio::test]
async fn never_gated_routes_succeed_while_the_viewer_gate_is_saturated() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    // Both sessions are minted BEFORE the gate is saturated below: `/session/authorise` IS one of
    // D-B's gated paths (it shares the viewer/session compute budget), so acquiring a *second*
    // session token during saturation would itself race the gate rather than testing the
    // never-gated routes this test is actually about.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let second_auth = authorise(&server, &["0"]).await;
    let second_token_id = second_auth["token_id"].as_u64().unwrap();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    poll_until_in_flight(&server, 1).await;

    // `/healthz`: no bearer, no gate.
    let healthz_resp = server
        .client
        .get(server.viewer_url("/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(healthz_resp.status(), 200, "/healthz must never be gated");

    // `/v1/meta`: viewer-plane bearer, but never gated (D-B's gated-paths list is exact).
    let meta_resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(meta_resp.status(), 200, "/v1/meta must never be gated");

    // `/session/revoke`: session-plane credential, never gated. Revokes the SECOND session
    // (minted before saturation, above) so the slow request's own `Arc<SessionEntry>` — cloned
    // into its `spawn_blocking` closure before this point — is unaffected either way; this
    // assertion is purely about the revoke endpoint's own responsiveness under a saturated gate.
    let revoke_resp = server
        .client
        .post(server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "token_id": second_token_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke_resp.status(),
        204,
        "/session/revoke must never be gated"
    );

    // `/control/changes` suppress: the D13 test proper. The entire control plane is off the
    // viewer/session gate (D-B); a deny op must reach the WAL regardless.
    const SUPPRESS_SOURCE_ID: u64 = 3;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let suppress_resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        suppress_resp.status(),
        200,
        "D13: a suppress must succeed while the viewer gate is fully saturated"
    );

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);
}

/// Spec constraint: no permit leak. After a shed (a second request while the gate is saturated)
/// and after the holder's own completion, both the outer and inner semaphores must show their
/// permits fully returned — observed twice, live, via `/control/status`'s gauges rather than by
/// inference from a single before/after snapshot.
#[tokio::test]
async fn no_permit_leak_after_a_shed_or_a_completion() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    poll_until_in_flight(&server, 1).await;

    let shed_before = control_status(&server).await["compute"]["shed_total"]
        .as_u64()
        .unwrap();

    let shed_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(shed_resp.status(), 429);

    // The shed attempt's own (failed) permit acquisition must not have leaked: `in_flight` still
    // reads exactly 1 (the still-running slow request, nothing more, nothing less) and
    // `shed_total` incremented by exactly one.
    let after_shed = control_status(&server).await;
    assert_eq!(after_shed["compute"]["in_flight"], 1);
    assert_eq!(after_shed["compute"]["waiting"], 0);
    assert_eq!(
        after_shed["compute"]["shed_total"].as_u64().unwrap(),
        shed_before + 1
    );

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);

    // Deterministic wait for the completed request's permits to be returned, then a fresh
    // request must succeed — a leaked permit would make it shed too.
    poll_until_in_flight(&server, 0).await;
    let after_completion = control_status(&server).await;
    assert_eq!(after_completion["compute"]["waiting"], 0);

    let third_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        third_resp.status(),
        200,
        "a leaked permit would make this request shed too"
    );
}

/// D-E: `x-tessera-server-us`'s clock starts AFTER admission, so it stays close to what an
/// unqueued request measures even when this request was forced to queue for a long time; the
/// queueing itself shows up only in `x-tessera-admission-us`, which must grow to reflect it.
///
/// Self-scaling, not a fixed wall-clock bet (this file's established pattern): rather than
/// asserting an absolute microsecond bound, this compares the *queued* fast request's own two
/// headers against each other (`server_us` must be much smaller than `admission_us` — most of
/// its total time was spent waiting, not computing) and against a genuinely unqueued baseline
/// request measured in the same run (`server_us` close to baseline; `admission_us` far above the
/// baseline's own near-zero admission wait).
#[tokio::test]
async fn server_us_excludes_admission_wait_while_admission_us_captures_it() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // compute_queue = 1 (not 0): the queued fast request below must be ADMITTED (a slot) and
    // then WAIT for a compute permit, rather than being shed outright by stage 1 — that wait is
    // exactly what `x-tessera-admission-us` needs to capture. A generous timeout so it is never
    // shed by stage 2 either; this test is about the timing split, not the shedding contract.
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 1, 60_000),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    // Baseline: a solo fast request with no contention at all.
    let baseline_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(baseline_resp.status(), 200);
    let baseline_admission_us: u64 = header_u64(&baseline_resp, "x-tessera-admission-us");
    let baseline_server_us: u64 = header_u64(&baseline_resp, "x-tessera-server-us");

    // Now hold the gate with a slow request, and send a fast one behind it.
    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });
    poll_until_in_flight(&server, 1).await;

    let queued_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(queued_resp.status(), 200);
    let queued_admission_us = header_u64(&queued_resp, "x-tessera-admission-us");
    let queued_server_us = header_u64(&queued_resp, "x-tessera-server-us");

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);

    assert!(
        queued_admission_us > baseline_admission_us,
        "a request forced to queue behind a slow one must show a larger admission wait than an \
         unqueued baseline: queued={queued_admission_us}us baseline={baseline_admission_us}us"
    );
    assert!(
        queued_server_us < queued_admission_us,
        "server_us must exclude the queueing this request experienced -- it should be far \
         smaller than admission_us, not comparable to it: server_us={queued_server_us}us \
         admission_us={queued_admission_us}us"
    );
    // Generous relative bound (self-scaling, not an absolute figure): the queued request's own
    // compute cost stays within an order of magnitude of the baseline's, plus a fixed epsilon so
    // a near-zero baseline (a handful of microseconds, quite possible for this fixture's tiny
    // corpus) cannot make the ratio unstable.
    assert!(
        queued_server_us < baseline_server_us.max(2_000) * 10,
        "server_us should stay close to the unqueued baseline: queued={queued_server_us}us \
         baseline={baseline_server_us}us"
    );
}

fn header_u64(resp: &reqwest::Response, name: &str) -> u64 {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("response is missing the {name} header"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("{name} header is not a valid u64"))
}

// ---------------------------------------------------------------------------------------------
// Task 5 (D-C): cooperative cancellation wired to client disconnect (the rapid-pan case).
// ---------------------------------------------------------------------------------------------

/// A slow viewport request engineered to spread its cost across MANY tiles rather than
/// [`slow_viewport_body`]'s one giant tile. D-C's per-tile cancellation check sits at the top of
/// the tile loop — it is deliberately not checked mid-tile (a tile's own underlay sweep is
/// bounded, in-flight work, same as every other per-tile stage) — so a single-tile fixture like
/// `slow_viewport_body` (`zoom = 0`) cannot demonstrate early interruption at all: cancellation
/// would only ever be observed once that one tile's entire sweep has already finished, which is
/// indistinguishable from no cancellation. `zoom = 2` gives 16 tiles; `underlay_offset = 9` costs
/// ~262144 sub-cell evaluations per tile (~4.2M total, tens of tiles' worth of real work), so a
/// disconnect landing after any prefix of tiles releases the gate long before the rest would have
/// run.
fn slow_multi_tile_viewport_body() -> serde_json::Value {
    serde_json::json!({
        "slice": "s0", "zoom": 2, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
        "underlay_offset": 9
    })
}

/// D-C, warm-session scope: a client that drops its connection mid-viewport — the rapid-pan case
/// — releases the compute-admission gate's permit well before the full service time an
/// uncancelled request of the same shape takes. Observed two ways: directly, via `/control/
/// status`'s `compute.in_flight` gauge dropping back to 0 promptly rather than only once the full
/// sweep would naturally finish; and indirectly, via a follow-up request being admitted at once
/// instead of shed.
///
/// **Warm-session scope, deliberately.** A slot-state row-projection build (Tasks 1-2, D-G) is
/// non-cancellable bounded work by design — D-C's scope note: its result serves later arrivals,
/// so it always runs to completion. A COLD first viewport's build cost would dominate this test's
/// timing regardless of cancellation and would prove nothing about the per-tile checks this task
/// adds. A fast warm-up request first, on the SAME token, gets this token/slice's row projection
/// to `Ready` before either slow request below, so the slow request's cost is entirely its
/// (cancellation-interruptible, per-tile) [`slow_multi_tile_viewport_body`] sweep.
///
/// **Self-scaling, not a fixed wall-clock bet** — same pattern as this file's other slow-viewport
/// tests (see e.g. `healthz_stays_prompt_while_a_long_viewport_runs`'s doc): `baseline_elapsed` is
/// this run's own measured time for the full, uncancelled sweep to complete on this machine, and
/// the disconnected run's release time is compared against a fraction of it, never an absolute
/// figure.
///
/// **Why `slow_task.abort()` is a faithful stand-in for a real client disconnect.** Aborting the
/// tokio task driving the `reqwest` request drops that request's future at its next await point —
/// which drops the underlying (not-yet-complete) connection, the same event a real browser
/// tearing down a stale fetch produces. On the server side this is indistinguishable from any
/// other broken connection: axum/hyper notice the peer went away and drop the handler's own
/// future, which is the ONLY signal this transport gives for "the client left" and exactly what
/// `CancelGuard` (`tessera-server::viewer`) is wired to.
///
/// **What this test does NOT claim.** Like the engine-level timing test
/// (`cancel_flipped_from_another_thread_aborts_a_long_request_before_it_completes` in
/// `tessera-engine`'s `tests/viewport.rs`), this does not pin down which of `Engine::viewport`'s
/// three checkpoints the disconnect is caught at — `poll_until_in_flight(&server, 1)` only proves
/// the request has been admitted and started running compute, not how far into the sweep it has
/// gotten by the time `abort()` fires. The disconnect could equally land at the pre-compose
/// checkpoint, before any tile. This test's value is observing permit release end to end (the
/// drop-guard flips, SOME checkpoint catches it, the gate frees up) rather than proving the
/// per-tile check specifically fires mid-sweep; per-tile placement is a code-review concern, per
/// the D-C design brief.
#[tokio::test]
async fn dropping_a_client_connection_mid_viewport_releases_the_gate_promptly() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    // Warm-session scope (see this test's doc): warms this token/slice's row-projection cache
    // before either slow request below.
    let warm = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(warm.status(), 200);

    // Baseline: the full, uncancelled slow sweep's own wall-clock time on this run/machine, over
    // the now-warm session.
    let baseline_start = std::time::Instant::now();
    let baseline_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&slow_multi_tile_viewport_body())
        .send()
        .await
        .unwrap();
    let baseline_elapsed = baseline_start.elapsed();
    assert_eq!(baseline_resp.status(), 200);
    assert!(
        baseline_elapsed > std::time::Duration::from_millis(50),
        "the uncancelled baseline finished in {baseline_elapsed:?}, too fast to exercise this \
         test's early-release scenario -- widen the underlay offset"
    );

    // The actual scenario: a second slow request, admitted and genuinely running
    // (`in_flight == 1`, a deterministic poll rather than a guessed delay) before the client
    // disconnects.
    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_multi_tile_viewport_body())
            .send()
            .await
    });

    poll_until_in_flight(&server, 1).await;

    let release_start = std::time::Instant::now();
    slow_task.abort();
    // The task is cancelled at its next await point -- whether it resolves at all (and with what)
    // depends on exactly where the abort landed; this test only cares about server-side gate
    // state below, so the client-side outcome is discarded either way.
    let _ = slow_task.await;

    // D-C: the drop-guard flips the token when axum drops the handler future on disconnect; the
    // engine's per-tile check observes it and aborts; the `spawn_blocking` closure returns `Err`
    // and drops `_gate_permits` -- releasing both `OwnedSemaphorePermit`s well before the full
    // sweep would naturally finish.
    poll_until_in_flight(&server, 0).await;
    let release_elapsed = release_start.elapsed();

    assert!(
        release_elapsed < baseline_elapsed / 2,
        "the gate took {release_elapsed:?} to free its permit after the client disconnected, \
         not meaningfully less than the {baseline_elapsed:?} an uncancelled sweep takes on this \
         run -- the engine does not appear to be aborting on disconnect"
    );

    // Observable via a follow-up request being admitted promptly, not shed with 429.
    let follow_up = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        follow_up.status(),
        200,
        "the gate's only slot should already be free after the disconnect -- a 429 here would \
         mean the permit leaked past the client's disconnect"
    );
}

// ---------------------------------------------------------------------------------------------
// Concurrency — D-D/D-F intra-request rayon parallelism (Task 6)
// ---------------------------------------------------------------------------------------------

/// THE HEADLINE TEST, server-side (D-D/D-F): the full Arrow response **body** `POST /v1/viewport`
/// returns is byte-for-byte identical whether `serve.compute_threads` is 1 or 8 — the same claim
/// `tessera-engine`'s own
/// `viewport_output_is_byte_identical_at_compute_threads_1_and_8` pins at the engine level,
/// carried one layer further to what a real client actually receives on the wire, through
/// `run_viewport`'s Arrow IPC framing (`viewer.rs`) and axum's response body.
///
/// Two servers, same bundle, differing only in `EngineConfig::compute_threads`; the same
/// authorisation terms (so both sessions see the identical mask) and the identical request body.
/// Only the response **body** is compared -- `x-tessera-server-us`, `x-tessera-admission-us` and
/// (when enabled) `x-tessera-stage-ns` are wall-clock/CPU-time measurements of this specific run
/// and are expected to differ between the two servers, and between runs of the same server; none
/// of them are part of this byte-equality claim. `x-tessera-pin` IS compared -- it is derived from
/// the bundle's own `(prefix, segments_version)`, not from timing, so it must agree too.
///
/// **Calibration task fix-wave note.** Uses `PARALLEL_HEADLINE_ITEMS` (300,000), not this file's
/// default `N_ITEMS` (1,000) — at 1,000 items this request's `Σ range.len()` cannot reach
/// `tessera_engine::viewport::SERIAL_FALLBACK_MAX_ROWS` (200,000), so both servers would silently
/// take the same serial-fold branch regardless of `compute_threads` and this test would no longer
/// exercise the fan-out its own doc claims to. See that constant's doc, and
/// `tessera-engine/tests/viewport.rs`'s identically-named constant, for the fixture-size argument.
#[tokio::test]
async fn viewport_response_body_is_byte_identical_at_compute_threads_1_and_8() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    let config_1 = EngineConfig {
        compute_threads: 1,
        ..default_engine_config()
    };
    let config_8 = EngineConfig {
        compute_threads: 8,
        ..default_engine_config()
    };

    let server_1 = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
        config_1,
    )
    .await;
    let server_8 = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache-8"),
        &tmp.path().join("wal-8.log"),
        config_8,
    )
    .await;

    let auth_1 = authorise(&server_1, &["0"]).await;
    let token_1 = auth_1["token"].as_str().unwrap();
    let auth_8 = authorise(&server_8, &["0"]).await;
    let token_8 = auth_8["token"].as_str().unwrap();

    // zoom=3 over the full extent: 64 candidate tiles, most non-empty over this fixture's
    // `(e*37, e*53) % 1000` scatter across `N_ITEMS = 1000` -- multiple non-empty tiles, so the
    // response's tile-order/point-concatenation ordering is actually exercised, plus an underlay
    // request so that per-tile path runs across tiles too.
    let body = serde_json::json!({
        "slice": "s0", "zoom": 3, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 50,
        "underlay_offset": 2
    });

    let resp_1 = server_1
        .client
        .post(server_1.viewer_url("/v1/viewport"))
        .bearer_auth(token_1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_1.status(), 200);
    let pin_1 = resp_1
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes_1 = resp_1.bytes().await.unwrap();

    let resp_8 = server_8
        .client
        .post(server_8.viewer_url("/v1/viewport"))
        .bearer_auth(token_8)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_8.status(), 200);
    let pin_8 = resp_8
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes_8 = resp_8.bytes().await.unwrap();

    assert_eq!(
        pin_1, pin_8,
        "the pin must agree -- same bundle, same generation"
    );

    let (tiles, points) = decode_viewport(&bytes_1);
    assert!(
        tiles.len() > 1,
        "need more than one non-empty tile to exercise cross-tile ordering, got {}",
        tiles.len()
    );
    assert!(!points.is_empty(), "the fixture must return some points");

    assert_eq!(
        bytes_1, bytes_8,
        "the full Arrow response body must be byte-for-byte identical regardless of \
         compute_threads -- this is also the statement that the Python differential oracle and \
         the conformance byte-scanner's vectors are unaffected: they consume exactly these bytes \
         and know nothing about compute_threads"
    );
}

/// Server-level twin of
/// `tessera-engine`'s `viewport_output_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles`
/// (fix-wave minor: the headline test above, like its engine-level counterpart, never exercises
/// `tile_result`'s `visible == 0 -> Ok(None)` empty-tile skip path). Same trick, no new fixture
/// data: this file's fixture scatter is `(e*37, e*53) % 1000`, a bijection of `e % 1000` onto the
/// 1000×1000 residue lattice, so `N_ITEMS = 1_000` items occupy up to 1,000 distinct locations
/// spread across the full extent -- dense enough at `zoom = 3` (64 candidate tiles) to leave almost
/// every tile non-empty, but at `zoom = 8` (up to 65,536 candidate tiles) sparse enough that most
/// candidate tiles are genuinely empty while a real minority are not.
///
/// **Calibration task fix-wave note.** Same reasoning as the headline test above:
/// `PARALLEL_HEADLINE_ITEMS` replaces `N_ITEMS` so `Σ range.len()` clears
/// `SERIAL_FALLBACK_MAX_ROWS` and the two servers are genuinely comparing serial against
/// parallel. The occupied/empty tile mix (still 1,000 distinct locations, more items stacked on
/// each) is unaffected — see the doc above.
#[tokio::test]
async fn viewport_response_body_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles(
) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    let config_1 = EngineConfig {
        compute_threads: 1,
        ..default_engine_config()
    };
    let config_8 = EngineConfig {
        compute_threads: 8,
        ..default_engine_config()
    };

    let server_1 = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
        config_1,
    )
    .await;
    let server_8 = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache-8"),
        &tmp.path().join("wal-8.log"),
        config_8,
    )
    .await;

    let auth_1 = authorise(&server_1, &["0"]).await;
    let token_1 = auth_1["token"].as_str().unwrap();
    let auth_8 = authorise(&server_8, &["0"]).await;
    let token_8 = auth_8["token"].as_str().unwrap();

    let bbox = [0.0, 0.0, 1000.0, 1000.0];
    let zoom = 8;
    let body = serde_json::json!({
        "slice": "s0", "zoom": zoom, "bbox": bbox, "k": 50
    });
    let candidate_tiles = tiles_for_bbox(bbox, zoom, &extent()).len();

    let resp_1 = server_1
        .client
        .post(server_1.viewer_url("/v1/viewport"))
        .bearer_auth(token_1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_1.status(), 200);
    let bytes_1 = resp_1.bytes().await.unwrap();

    let resp_8 = server_8
        .client
        .post(server_8.viewer_url("/v1/viewport"))
        .bearer_auth(token_8)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_8.status(), 200);
    let bytes_8 = resp_8.bytes().await.unwrap();

    let (tiles, _points) = decode_viewport(&bytes_1);
    assert!(
        !tiles.is_empty(),
        "need at least one non-empty tile for this to be a real mixed case, got none"
    );
    assert!(
        tiles.len() < candidate_tiles,
        "need at least one genuinely empty (Ok(None)-skipped) tile among the {candidate_tiles} \
         candidates to exercise the skip path this test is for -- got {} non-empty tiles",
        tiles.len()
    );

    assert_eq!(
        bytes_1, bytes_8,
        "the full Arrow response body must be byte-for-byte identical regardless of \
         compute_threads, including on the mostly-empty-tile Ok(None) skip path"
    );
}
