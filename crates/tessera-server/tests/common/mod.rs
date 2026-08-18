//! Shared fixtures for `tessera-server`'s integration tests.
//!
//! The integration tests are split by subject across several binaries — `http.rs` (viewer plane,
//! session plane, config, byte shape, the compute-admission gate), `http_write.rs` (the control
//! plane's write path) and `http_engine_state.rs` (pins and session revocation). This module holds
//! every fixture they share, so the split does not become drift.

// Each integration-test binary compiles this module separately, so a fixture used by only one of
// them is genuinely dead code in the other. Allowing it here is what keeps the two halves from
// each carrying their own copy — which is the drift this module exists to prevent.
#![allow(dead_code)]

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

use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::faults::FaultSwitchboard;
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, ComputeGate, IngestAdmission, SessionRegistry};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

pub const N_ITEMS: u64 = 1_000;
pub const SESSION_CREDENTIAL: &str = "session-secret";

pub const OPERATOR_CREDENTIAL: &str = "operator-secret";
/// Fixed test key, matching `tessera-build`'s own test fixtures — not sensitive, this repository
/// contains no real deployment key.
pub const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// The fixture bundle's `identity.idset` — contracts §2.2's idset, which
/// `/v1/meta` reports and `/v1/items` compares an optional `idset` against.
pub const FIXTURE_IDSET: u32 = 1;

pub fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

pub fn extent() -> Bounds {
    Bounds {
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
/// the byte-equality tests in `tests/http.rs` need a genuinely multi-tile, multi-thousand-row
/// regime, well above this module's default `N_ITEMS` — see that file's `PARALLEL_HEADLINE_ITEMS`
/// doc for how they reach the parallel branch specifically, which item count alone no longer does
/// post-§14).
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
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: points_path.to_path_buf(),
        corpus: Some(points_path.to_path_buf()),
        access: tessera_build::config::AccessInput::relation(pairs_path.to_path_buf()),
        out: out.to_path_buf(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        artifacts: None,
        artifact_members: None,
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
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
    /// The same `AppState` the three routers are serving, so a test can assert on an engine-side
    /// observable a handler was supposed to move — not just on the status code it returned.
    ///
    /// Without it, `revoke_prunes_the_token` could assert only a 204 and a survivor's 200 — which
    /// covers nothing its name claims: deleting `state.engine.prune_token(...)` from the revoke
    /// handler leaves every status code in this crate's tests unchanged. `/control/status` does
    /// publish the cache gauges, but reading them costs the operator credential and a JSON parse;
    /// this is the direct route from an HTTP test to a cache observable.
    ///
    /// **Not a licence to bypass HTTP.** A test that drives the engine through this field instead
    /// of through a request has stopped being a server test; the point is to *observe* after
    /// driving the request normally.
    pub state: Arc<AppState>,
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

/// The `EngineConfig` every test but the concurrency ones uses.
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
        // tessera-engine's `tests/common/mod.rs`.
        flush_max_age_secs: 90,
        max_merged_segment_bytes: None,
        tier_width: None,
        segment_floor_bytes: None,
        coalesce_width: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
    }
}

/// [`spawn_server`], with a caller-chosen streamed-viewport flush threshold and write-stall
/// budget — the two knobs the streaming tests pin (a tiny flush for the multi-frame path, a
/// short stall for the shed path).
pub async fn spawn_server_with_stream_flush(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    stream_flush_bytes: usize,
    stream_write_stall_ms: u64,
) -> TestServer {
    let config = default_engine_config();
    let max_k = config.max_k;
    let engine = Engine::open(bundle_root, cache_dir, wal_path, Passthrough::new(), config)
        .expect("engine should open against a freshly built bundle");
    mount_server_with_flush(
        engine,
        max_k,
        generous_test_gate(),
        generous_ingest_limits(),
        Vec::new(),
        stream_flush_bytes,
        stream_write_stall_ms,
        Arc::new(FaultSwitchboard::new()),
    )
    .await
}

pub async fn spawn_server(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> TestServer {
    spawn_server_with_config(bundle_root, cache_dir, wal_path, default_engine_config()).await
}

/// The default test gate: generous enough that no test which issues a handful of sequential
/// requests can ever observe it. Only the gate-specific tests construct a deliberately tiny
/// [`ComputeGate`] to exercise shedding.
pub fn generous_test_gate() -> ComputeGate {
    ComputeGate::new(64, 64, 250)
}

/// Like [`spawn_server`], but with a caller-supplied `EngineConfig` — the concurrency tests
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

/// Like [`spawn_server_with_config`], but also with a caller-supplied [`ComputeGate`] — the
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
    spawn_server_from_engine(engine, max_k, compute_gate).await
}

/// The "wrap an already-constructed `Engine` into a running three-listener server" half of
/// [`spawn_server_with_config_and_gate`], factored out so the byte-equality tests
/// can construct their own `Engine` (to call `set_serial_fallback_max_rows_for_test` on it, which
/// needs the owned `Engine` before it is moved into `AppState`) while still reusing the router/
/// listener plumbing every other test goes through.
pub async fn spawn_server_from_engine(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
) -> TestServer {
    // The WAL lives on its own executor thread, and `prepare` starts it for a real server. Every
    // server test reaches its engine through this one function, so starting it here is what keeps
    // `/control/ingest` and `/control/changes` working in the test harness.
    //
    // The bound is generous on purpose: no test reaching this helper means to exercise queue-full
    // backpressure (`ingest_429s_when_the_queue_is_full` sets its own), and a small bound would turn
    // an unrelated timing wobble into a spurious 429.
    let mut engine = engine;
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    mount_server(engine, max_k, compute_gate).await
}

/// The listener/router/`AppState` half of [`spawn_server_from_engine`], for an engine whose write
/// executor the caller has already dealt with — started with its own bound, started with faults, or
/// **deliberately not started at all**.
///
/// Split out for the readiness tests, which need the last of those: `/readyz` must answer 503 for an
/// engine with no executor, and `spawn_server_from_engine` starts one unconditionally (and would
/// panic on `AlreadyStarted` if a test started its own first).
pub async fn mount_server(engine: Engine, max_k: usize, compute_gate: ComputeGate) -> TestServer {
    mount_server_with(
        engine,
        max_k,
        compute_gate,
        generous_ingest_limits(),
        Vec::new(),
    )
    .await
}

/// The control-plane bounds, as a caller-supplied set.
///
/// `admission` bounds concurrent `/control/ingest` handlers (and therefore the blocking-pool
/// threads ingest can hold); `max_batch_rows` and `max_batch_bytes` are the two 422 caps. They are
/// *different quantities* from the write executor's `ingest_queue_bound`, which is chosen at
/// `start_write_executor` — a handler holds a thread through a window in which it holds no queue
/// slot at all — so a test that wants one of the two 429s must set both deliberately.
pub struct IngestLimits {
    pub admission: usize,
    pub max_batch_rows: usize,
    pub max_batch_bytes: usize,
    /// Buffer occupancy at which ingest is refused (§1.3).
    pub buffer_max_items: usize,
}

/// Generous enough that no test which is not about these bounds can observe them — the same
/// principle as [`generous_test_gate`]. Only the bound-specific tests set their own.
pub fn generous_ingest_limits() -> IngestLimits {
    IngestLimits {
        admission: 64,
        // Above the *production* default of 10 000, deliberately:
        // `concurrent_ingests_do_not_delay_a_control_changes_suppress` posts 40 000-row batches to
        // make the ingest side genuinely heavy. A harness default that silently turned that test's
        // premise into a 422 would be measuring the harness.
        max_batch_rows: 200_000,
        // Above anything a test that is not about this bound could reach: without a flush the
        // buffer only grows, so a modest ceiling would turn every long-running ingest test into a
        // 429 about a bound it never meant to exercise.
        buffer_max_items: 10_000_000,
        max_batch_bytes: 64 * 1024 * 1024,
    }
}

/// As [`mount_server`], with the control-plane bounds chosen by the caller.
pub async fn mount_server_with_ingest_limits(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
) -> TestServer {
    mount_server_with(engine, max_k, compute_gate, ingest_limits, Vec::new()).await
}

/// Like [`spawn_server`], but with `serve.dev_cors_origins` set — `tests/cors.rs` only.
///
/// A separate entry point rather than a parameter on the existing ones: `Engine` is not `Clone`,
/// so a test cannot re-mount an already-serving one, and widening `mount_server`'s signature would
/// churn every call site in `tests/http.rs` and `tests/http_write.rs` for a key none of them care
/// about.
pub async fn spawn_server_with_cors(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    dev_cors_origins: Vec<String>,
) -> TestServer {
    let config = default_engine_config();
    let max_k = config.max_k;
    let engine = Engine::open(bundle_root, cache_dir, wal_path, Passthrough::new(), config)
        .expect("engine should open against a freshly built bundle");
    mount_server_with(
        engine,
        max_k,
        generous_test_gate(),
        generous_ingest_limits(),
        dev_cors_origins,
    )
    .await
}

/// The shared body of the three entry points above, with **both** parameter sets explicit.
///
/// The ingest bounds and `serve.dev_cors_origins` each have a parameterised variant of
/// `mount_server`. Rather than nest one inside the other, both delegate to this: each named entry
/// point keeps its own defaults, and a test that needs both calls this directly.
async fn mount_server_with(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
    dev_cors_origins: Vec<String>,
) -> TestServer {
    mount_server_with_flush(
        engine,
        max_k,
        compute_gate,
        ingest_limits,
        dev_cors_origins,
        1 << 20,
        10_000,
        Arc::new(FaultSwitchboard::new()),
    )
    .await
}

/// As [`mount_server`], for an engine the caller started with
/// `start_write_executor_with_faults` — the **same** `Arc` must be handed here, or
/// `/control/faults/*` arms a board no thread ever consults. The one entry point behind the
/// faults-surface tests; every other mount stores a fresh, disarmed board, which is inert.
pub async fn mount_server_with_faults(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    faults: Arc<FaultSwitchboard>,
) -> TestServer {
    mount_server_with_flush(
        engine,
        max_k,
        compute_gate,
        generous_ingest_limits(),
        Vec::new(),
        1 << 20,
        10_000,
        faults,
    )
    .await
}

/// [`mount_server_with`], with the streamed viewport's flush threshold and write-stall budget
/// explicit — the tests that pin the multi-frame and shed paths mount a threshold far below one
/// response's bytes and a stall far below the default.
#[allow(clippy::too_many_arguments)]
async fn mount_server_with_flush(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
    dev_cors_origins: Vec<String>,
    stream_flush_bytes: usize,
    stream_write_stall_ms: u64,
    faults: Arc<FaultSwitchboard>,
) -> TestServer {
    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        max_k,
        // Small enough that the fixtures' vocabularies page rather than arriving whole, so the
        // cursor is exercised by an ordinary request rather than only by a contrived one.
        max_category_values: 4,
        compute_gate,
        ingest_admission: IngestAdmission::new(ingest_limits.admission),
        ingest_max_batch_rows: ingest_limits.max_batch_rows,
        ingest_buffer_max_items: ingest_limits.buffer_max_items,
        ingest_max_batch_bytes: ingest_limits.max_batch_bytes,
        // On, so the header assertions below exercise the emission path rather than only its
        // absence. The compile-time `bench-timing` gate still decides whether anything is sent.
        stage_timing: true,
        // The op-point default (1 MiB) leaves every fixture-sized response in one points frame,
        // which is exactly the degenerate case contracts §3.2 requires readers to accept; the
        // multi-frame path is exercised by the tests that mount a tiny threshold explicitly
        // (`spawn_server_with_stream_flush`).
        stream_flush_bytes,
        stream_write_stall_ms,
        stream_deadline_ms: 60_000,
        session_credential: SESSION_CREDENTIAL.to_string(),
        operator_credential: OPERATOR_CREDENTIAL.to_string(),
        dev_cors_origins,
        faults,
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
        state,
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

/// `POST /v1/items/{tessera_id}` with no body fields set (no pin, no idset).
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
pub type PointRow = (u64, u64);

/// One decoded `/v1/viewport` response body, every frame kind (contracts §3.2 r26).
pub struct DecodedViewport {
    /// `(tile, visible, matched)` per row — `served` is asserted where a test needs it, via
    /// [`decode_viewport_frames`]' `served` field.
    pub tiles: Vec<TileRow>,
    pub served: Vec<u64>,
    /// `(tessera_id, code)` per point, concatenated across every kind-3 frame in order.
    pub points: Vec<PointRow>,
    /// `(cell, count)` — `None` when no kind-2 frame was present (underlay unrequested).
    pub sub_cells: Option<Vec<(u64, u64)>>,
    /// The kind-5 artifacts frame. `None` only if the response carried no artifacts channel at
    /// all — a served response always carries one, empty or not, so `Some(vec![])` and `None` are
    /// different facts and a test may assert on either.
    pub artifacts: Option<Vec<ArtifactRow>>,
    /// The kind-4 trailer, parsed. Its key set is asserted here — the one server-authored JSON
    /// region of the body must not quietly acquire a field the comparator never sees
    /// (`streamed-serving.md` §7).
    pub trailer: serde_json::Value,
    /// How many kind-3 frames the body carried — the chunking, which is NOT contract, but which
    /// the multi-frame tests pin against their configured flush threshold.
    pub point_frames: usize,
    /// The body minus the trailer frame: the deterministic region, what byte-equality
    /// assertions compare (`streamed-serving.md` §7).
    pub deterministic_bytes: Vec<u8>,
}

/// One row of the kind-5 artifacts frame, as a test reads it back.
///
/// `PartialEq` and not `Eq`: a centroid is a mean and travels as `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactRow {
    pub layer: String,
    pub tessera_id: u64,
    pub stable_key: Option<String>,
    /// **How many members this principal can see** — never how many the artifact has.
    pub masked_count: u64,
    /// Derived geometry, in grid units, computed over the members this principal can see. `None`
    /// is *the layer declares none* and never *withheld*.
    pub centroid: Option<[f64; 2]>,
    pub bbox: Option<[u32; 4]>,
    pub hull: Option<Vec<[u32; 2]>>,
}

fn str_col(
    batch: &arrow::record_batch::RecordBatch,
    i: usize,
) -> arrow::array::StringArray {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap()
        .clone()
}

fn u64_col(batch: &arrow::record_batch::RecordBatch, i: usize) -> UInt64Array {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap()
        .clone()
}

/// Decode a complete streamed `/v1/viewport` body: walk the tagged length-prefixed frames
/// (contracts §3.2 r26), decode each payload as the complete Arrow IPC stream (or trailer JSON)
/// it is, and enforce the frame grammar — tiles first, exactly one trailer last, unknown kinds
/// refused (`tessera_wire::split_frames`).
pub fn decode_viewport_frames(bytes: &[u8]) -> DecodedViewport {
    let frames = tessera_wire::split_frames(bytes).expect("well-formed frame sequence");
    assert!(!frames.is_empty(), "a response carries at least tiles + trailer");
    assert_eq!(
        frames.first().unwrap().0,
        tessera_wire::FRAME_TILES,
        "the tiles frame is first"
    );
    assert_eq!(
        frames.last().unwrap().0,
        tessera_wire::FRAME_TRAILER,
        "the trailer frame is last — its presence is the completeness signal"
    );

    let mut tiles = Vec::new();
    let mut served = Vec::new();
    let mut points = Vec::new();
    let mut sub_cells: Option<Vec<(u64, u64)>> = None;
    let mut artifacts: Option<Vec<ArtifactRow>> = None;
    let mut trailer: Option<serde_json::Value> = None;
    let mut point_frames = 0usize;
    let mut deterministic_end = 0usize;
    let mut at = 0usize;

    for (index, (kind, payload)) in frames.iter().enumerate() {
        let frame_len = tessera_wire::FRAME_HEADER_BYTES + payload.len();
        match *kind {
            tessera_wire::FRAME_TILES => {
                assert_eq!(index, 0, "exactly one tiles frame, first");
                let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let tile = u64_col(&batch, 0);
                    let visible = u64_col(&batch, 1);
                    let matched = u64_col(&batch, 2);
                    let served_col = u64_col(&batch, 3);
                    for i in 0..batch.num_rows() {
                        tiles.push((tile.value(i), visible.value(i), matched.value(i)));
                        served.push(served_col.value(i));
                    }
                }
                deterministic_end = at + frame_len;
            }
            tessera_wire::FRAME_SUB_CELLS => {
                assert!(sub_cells.is_none(), "at most one sub-cells frame");
                assert_eq!(index, 1, "the sub-cells frame immediately follows tiles");
                let cells = sub_cells.get_or_insert_with(Vec::new);
                let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let cell = u64_col(&batch, 0);
                    let count = u64_col(&batch, 1);
                    for i in 0..batch.num_rows() {
                        cells.push((cell.value(i), count.value(i)));
                    }
                }
                deterministic_end = at + frame_len;
            }
            tessera_wire::FRAME_ARTIFACTS => {
                assert!(artifacts.is_none(), "exactly one artifacts frame");
                let rows = artifacts.get_or_insert_with(Vec::new);
                let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let layer = str_col(&batch, 0);
                    let tessera_id = u64_col(&batch, 1);
                    let stable_key = str_col(&batch, 2);
                    let masked_count = u64_col(&batch, 3);
                    let f64_at = |col: usize, i: usize| {
                        let a = batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::Float64Array>()
                            .unwrap();
                        a.is_valid(i).then(|| a.value(i))
                    };
                    let u32_at = |col: usize, i: usize| {
                        let a = batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::UInt32Array>()
                            .unwrap();
                        a.is_valid(i).then(|| a.value(i))
                    };
                    let hull_axis = |col: usize, i: usize| {
                        let a = batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::ListArray>()
                            .unwrap();
                        a.is_valid(i).then(|| {
                            let values = a.value(i);
                            let values = values
                                .as_any()
                                .downcast_ref::<arrow::array::UInt32Array>()
                                .unwrap();
                            (0..values.len()).map(|k| values.value(k)).collect::<Vec<_>>()
                        })
                    };
                    for i in 0..batch.num_rows() {
                        let hull = match (hull_axis(10, i), hull_axis(11, i)) {
                            (Some(xs), Some(ys)) => {
                                Some(xs.into_iter().zip(ys).map(|(x, y)| [x, y]).collect())
                            }
                            (None, None) => None,
                            _ => panic!("a hull with one axis and not the other"),
                        };
                        rows.push(ArtifactRow {
                            layer: layer.value(i).to_string(),
                            tessera_id: tessera_id.value(i),
                            stable_key: stable_key
                                .is_valid(i)
                                .then(|| stable_key.value(i).to_string()),
                            masked_count: masked_count.value(i),
                            centroid: f64_at(4, i)
                                .map(|x| [x, f64_at(5, i).expect("both axes or neither")]),
                            bbox: u32_at(6, i).map(|min_x| {
                                [
                                    min_x,
                                    u32_at(7, i).unwrap(),
                                    u32_at(8, i).unwrap(),
                                    u32_at(9, i).unwrap(),
                                ]
                            }),
                            hull,
                        });
                    }
                }
                deterministic_end = at + frame_len;
            }
            tessera_wire::FRAME_POINTS => {
                point_frames += 1;
                let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let tessera_id = u64_col(&batch, 0);
                    let code = u64_col(&batch, 1);
                    for i in 0..batch.num_rows() {
                        points.push((tessera_id.value(i), code.value(i)));
                    }
                }
                deterministic_end = at + frame_len;
            }
            tessera_wire::FRAME_TRAILER => {
                assert!(trailer.is_none(), "exactly one trailer");
                let parsed: serde_json::Value = serde_json::from_slice(payload).unwrap();
                // The closed key set (contracts §3.2 r26): `stage_ns` is the one optional key
                // (double-gated); everything else is exact.
                let object = parsed.as_object().expect("trailer is a JSON object");
                let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
                keys.retain(|k| *k != "stage_ns");
                keys.sort_unstable();
                assert_eq!(
                    keys,
                    vec!["arrow_serialise_ns", "flushes", "points", "stream_us"],
                    "the trailer's key set is closed"
                );
                trailer = Some(parsed);
            }
            other => panic!("split_frames returned an unknown kind {other}"),
        }
        at += frame_len;
    }

    let trailer = trailer.expect("trailer asserted present above");
    // Cross-check the counts the trailer claims against what the body actually carried.
    assert_eq!(
        trailer["points"].as_u64().unwrap(),
        points.len() as u64,
        "trailer points total matches the body"
    );
    assert_eq!(
        trailer["flushes"].as_u64().unwrap(),
        point_frames as u64,
        "trailer flush count matches the body"
    );
    // The r7 invariant, now asserted at the reader: served splits the concatenated points.
    assert_eq!(
        served.iter().sum::<u64>(),
        points.len() as u64,
        "sum of served equals the points delivered"
    );

    DecodedViewport {
        tiles,
        served,
        points,
        sub_cells,
        artifacts,
        trailer,
        point_frames,
        deterministic_bytes: bytes[..deterministic_end].to_vec(),
    }
}

/// The two-tuple view most tests want: `(tiles, points)` with tiles as `(tile, visible,
/// matched)` and points as `(tessera_id, code)` — the same shape the pre-streaming decoder
/// returned, over the framed body.
pub fn decode_viewport(bytes: &[u8]) -> (Vec<TileRow>, Vec<PointRow>) {
    let decoded = decode_viewport_frames(bytes);
    (decoded.tiles, decoded.points)
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

/// An Arrow ingest batch whose `external_id` column is nullable — contracts §3.4 r6 makes the
/// external id optional, and an item ingested without one is addressable only by its `tessera_id`.
///
/// Shared rather than copied: two binaries build this body, and a schema that drifted between them
/// would fail as a server-side parse error rather than as a test disagreement.
pub fn build_ingest_batch_optional(rows: &[(Option<&[u8]>, f32, f32, &str)]) -> Vec<u8> {
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
