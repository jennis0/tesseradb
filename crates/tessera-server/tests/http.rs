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
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, SessionRegistry};
use tessera_spatial::Extent;
use tessera_types::IdentityKey;

const N_ITEMS: u64 = 1_000;
const SESSION_CREDENTIAL: &str = "session-secret";
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
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        identity_epoch: FIXTURE_EPOCH,
        shard_id: 0,
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
            k_min: 2,
            k_max_marks: 200,
            // Saturate theta: these tests assert HTTP shape and masking, not density. See
            // tessera-engine's tests/viewport.rs `config()` for the full reasoning.
            theta_target_marks: u64::MAX,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
        },
    )
    .expect("engine should open against a freshly built bundle");

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        max_k: 200,
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
            19,
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
        assert_eq!(
            materialised, N_ITEMS,
            "F1 over the wire: selection materialises every visible row to return k"
        );
    } else {
        assert!(
            header.is_none(),
            "without the bench-timing feature the header must be absent even when \
             `stage_timing = true` — a release build must not emit it"
        );
    }
}
