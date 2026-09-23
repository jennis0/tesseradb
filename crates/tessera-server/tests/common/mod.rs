//! What `tessera-server`'s integration tests share: building a bundle, serving it, writing to
//! it, waiting for what a write publishes, and decoding what a viewer is served. A helper more
//! than one test file needs lives here and nowhere else.

// Each test binary compiles this module on its own, so a helper one binary does not use is dead
// code there.
#![allow(dead_code)]

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, Float32Array, Float64Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use parking_lot::Mutex;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

pub use tessera_build::config::AccessInput;
use tessera_build::{build, BuildArgs, ViewArgs};
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::faults::FaultSwitchboard;
use tessera_plugin::Passthrough;
use tessera_server::state::{AppState, ComputeGate, IngestAdmission, ServeLimits, SessionRegistry};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

pub const N_ITEMS: u64 = 1_000;
pub const SESSION_CREDENTIAL: &str = "session-secret";

pub const OPERATOR_CREDENTIAL: &str = "operator-secret";
/// The identity key `tessera-build`'s own test fixtures use. It guards nothing.
pub const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// The fixture bundle's idset, which `/v1/meta` reports and `/v1/items` compares a request's
/// `idset` against.
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

/// The whole-world Web Mercator frame, which is the unit square: every projection's output is
/// normalised to `[0, 1]` on both axes.
pub fn world_frame() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// A view group's quantisation over [`extent`].
pub fn group_frame() -> tessera_build::Quantisation {
    let e = extent();
    tessera_build::Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

pub fn terms_of(source_id: u64) -> Vec<u64> {
    if source_id.is_multiple_of(3) {
        vec![0, 1]
    } else {
        vec![0]
    }
}

/// Write `columns`, in order, as one parquet file at `path`.
pub fn write_parquet(path: &Path, columns: Vec<(Field, ArrayRef)>) {
    let (fields, arrays): (Vec<Field>, Vec<ArrayRef>) = columns.into_iter().unzip();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Write the points `ids` as a parquet file at `path`: `entity_id`, `x` and `y` placed by
/// `position`, then the `extra` columns.
pub fn write_points(
    path: &Path,
    ids: &[u64],
    position: impl Fn(u64) -> (f64, f64),
    extra: Vec<(Field, ArrayRef)>,
) {
    let (xs, ys): (Vec<f64>, Vec<f64>) = ids.iter().map(|&e| position(e)).unzip();
    let mut columns = vec![
        column("entity_id", false, UInt64Array::from(ids.to_vec())),
        column("x", false, Float64Array::from(xs)),
        column("y", false, Float64Array::from(ys)),
    ];
    columns.extend(extra);
    write_parquet(path, columns);
}

/// A column named `name` for [`write_points`] or [`write_parquet`], typed by its array.
pub fn column(name: &str, nullable: bool, array: impl Array + 'static) -> (Field, ArrayRef) {
    (
        Field::new(name, array.data_type().clone(), nullable),
        Arc::new(array),
    )
}

/// Where the fixture places entity `e` in [`extent`].
pub fn scatter(e: u64) -> (f64, f64) {
    (((e * 37) % 1000) as f64, ((e * 53) % 1000) as f64)
}

/// Entities `0..n`, placed by [`scatter`], with no other column.
pub fn write_points_n(path: &Path, n: u64) {
    write_points(path, &(0..n).collect::<Vec<_>>(), scatter, Vec::new());
}

/// The pairs relation over `rows`: each entity holds the terms [`terms_of`] gives it.
pub fn write_pairs(path: &Path, rows: &[u64]) {
    let (entities, terms): (Vec<u64>, Vec<u32>) = rows
        .iter()
        .flat_map(|&e| terms_of(e).into_iter().map(move |t| (e, t as u32)))
        .unzip();
    write_parquet(
        path,
        vec![
            column("entity_id", false, UInt64Array::from(entities)),
            column("term_id", false, UInt32Array::from(terms)),
        ],
    );
}

/// [`write_pairs`] over the entities `0..n`.
pub fn write_pairs_n(path: &Path, n: u64) {
    write_pairs(path, &(0..n).collect::<Vec<_>>());
}

/// Entities `0..places.len()`, each at its `(lon, lat)`, as a parquet file at `path`.
pub fn write_lon_lat(path: &Path, places: &[(f64, f64)]) {
    let (lons, lats): (Vec<f64>, Vec<f64>) = places.iter().copied().unzip();
    write_parquet(
        path,
        vec![
            column(
                "entity_id",
                false,
                UInt64Array::from_iter_values(0..places.len() as u64),
            ),
            column("lon", false, Float64Array::from(lons)),
            column("lat", false, Float64Array::from(lats)),
        ],
    );
}

/// The standard fixture: `n` items placed by [`scatter`], with the terms [`terms_of`] gives them,
/// built into `dir/bundle` from points and pairs files written beside it in `dir`.
pub fn build_fixture(dir: &Path, n: u64) -> std::path::PathBuf {
    build_fixture_with_access(dir, n, AccessInput::relation(dir.join("pairs.parquet")))
}

/// [`build_fixture`] under the `point_visibility` given, whose default is what an ingested row
/// with an empty `access` list takes.
pub fn build_fixture_with_access(dir: &Path, n: u64, access: AccessInput) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let points = dir.join("points.parquet");
    write_points_n(&points, n);
    write_pairs_n(&dir.join("pairs.parquet"), n);
    let out = dir.join("bundle");
    let view = view_args("s0", &points, access);
    build(&build_args(&out, vec![view])).expect("fixture build should succeed");
    out
}

/// Copy the bundle `build` writes into a directory's `bundle` to `dir/bundle`, building it on the
/// first call in this test binary and keeping it in `once`. Serving a bundle writes nothing into
/// it, but the write executor locks it, so each test that serves one serves its own copy.
pub fn copy_built<R>(
    once: &'static std::sync::OnceLock<TempDir>,
    dir: &Path,
    build: impl FnOnce(&Path) -> R,
) -> std::path::PathBuf {
    let built = once.get_or_init(|| {
        let tmp = TempDir::new_in(fixtures_dir()).unwrap();
        build(tmp.path());
        tmp
    });
    let out = dir.join("bundle");
    copy_dir(&built.path().join("bundle"), &out);
    out
}

/// This process's directory for built fixtures, `<pid>` beside a `<pid>.lock` this process holds
/// locked for as long as it runs. A static is never dropped, so the directories of processes that
/// have exited are removed here instead: a lock file nobody holds, and a directory whose lock file
/// is gone, which only a removal after its process exited leaves.
fn fixtures_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<(std::path::PathBuf, std::fs::File)> =
        std::sync::OnceLock::new();
    let (dir, _held) = DIR.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tessera-server-fixtures");
        std::fs::create_dir_all(&root).unwrap();
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "lock") {
                let unheld = std::fs::File::open(&path).is_ok_and(|f| f.try_lock().is_ok());
                if unheld {
                    let _ = std::fs::remove_dir_all(path.with_extension(""));
                    let _ = std::fs::remove_file(&path);
                }
            } else if path.is_dir() && !path.with_extension("lock").exists() {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
        // Locked before it takes its name, so no other process can find it unheld.
        let lock = tempfile::NamedTempFile::new_in(&root).unwrap();
        lock.as_file().lock().unwrap();
        let name = std::process::id().to_string();
        let held = lock.persist(root.join(format!("{name}.lock"))).unwrap();
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        (dir, held)
    });
    dir.clone()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &target),
            false => {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}

static STANDARD: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();

/// [`build_fixture`] of [`N_ITEMS`] into `dir`, built once per test binary and copied.
pub fn standard_fixture(dir: &Path) -> std::path::PathBuf {
    copy_built(&STANDARD, dir, |built| build_fixture(built, N_ITEMS))
}

/// Serve a copy of the standard fixture ([`standard_fixture`]) from `dir`.
pub async fn serve_standard(dir: impl AsRef<Path>) -> TestServer {
    standard_fixture(dir.as_ref());
    open(dir).await
}

/// A build of `views` into `out` under the test identity key, minting external ids and writing no
/// oracle pairs, with every other input empty. A test sets what it varies with struct update
/// syntax.
pub fn build_args(out: &Path, views: Vec<ViewArgs>) -> BuildArgs {
    BuildArgs {
        views,
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
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
        schema: Default::default(),
    }
}

/// A plain view `view_id` over the points at `points` with its access terms from `access`, in
/// [`extent`] with no projection.
pub fn view_args(view_id: &str, points: &Path, access: AccessInput) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: view_id.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access,
    }
}

/// Build one plain view, `s0`, over `points` and `pairs` into `out`, declaring the attributes and
/// vocabularies in `schema_toml` and reading each attribute from `points`.
pub fn build_declared(out: &Path, points: &Path, pairs: &Path, schema_toml: &str) {
    let schema_path = points.with_file_name("schema.toml");
    std::fs::write(&schema_path, schema_toml).unwrap();
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .unwrap()
        .schema;
    build(&BuildArgs {
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        schema,
        ..build_args(
            out,
            vec![view_args("s0", points, AccessInput::relation(pairs))],
        )
    })
    .expect("fixture build should succeed");
}

/// Build `n` items placed by [`scatter`], each with a non-null `f32` `score` of `e % 7`, into
/// `dir/bundle` under the attributes and vocabularies `schema_toml` declares.
pub fn build_scored(dir: &Path, n: u64, schema_toml: &str) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..n).collect();
    let score = Float32Array::from_iter_values(ids.iter().map(|e| (e % 7) as f32));
    write_points(&points, &ids, scatter, vec![column("score", false, score)]);
    write_pairs_n(&pairs, n);
    let out = dir.join("bundle");
    build_declared(&out, &points, &pairs, schema_toml);
    out
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
    /// The state the three routers serve, so a test can observe what a request changed in the
    /// engine. A test drives the server by HTTP and only reads through this.
    pub state: Arc<AppState>,
    /// The three `axum::serve` tasks. Each holds the state, and so the engine and its lock on the
    /// bundle root, until it is stopped.
    pub serve_tasks: Vec<tokio::task::JoinHandle<()>>,
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

    /// Stop serving and wait until the engine is dropped, so the bundle can be opened again.
    ///
    /// The state is held by this value, the three accept loops and one task per live connection.
    /// Replacing the client closes its connections, the accept loops are aborted, and this waits
    /// to hold the last reference before dropping it.
    pub async fn shutdown(mut self) {
        // A fresh client in place of this one: dropping the old pool closes its keep-alive
        // connections, and with them the per-connection tasks holding the state.
        self.client = reqwest::Client::new();
        let tasks = std::mem::take(&mut self.serve_tasks);
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        wait_until(
            "the connection tasks releasing the engine",
            std::time::Duration::from_secs(30),
            async || Arc::strong_count(&self.state) == 1,
        )
        .await;
        // The engine is dropped here: the executor thread is joined and the bundle root's write
        // lock released before this returns.
        drop(self);
    }
}

impl Drop for TestServer {
    /// Stops the accept loops. It cannot wait for the engine to drop; a test that reopens the
    /// bundle calls [`TestServer::shutdown`].
    fn drop(&mut self) {
        for task in &self.serve_tasks {
            task.abort();
        }
    }
}

/// The `EngineConfig` every test but the concurrency ones uses.
pub fn default_engine_config() -> EngineConfig {
    EngineConfig {
        token_max_lifetime_secs: 3600,
        max_k: 200,
        k_min: 2,
        k_max_marks: 200,
        // No density thinning: these tests assert what is served and masked, not density.
        theta_target_marks: u64::MAX,
        max_underlay_offset: 4,
        max_underlay_cells: 8192,
        max_tiles_per_request: 262_144,
        compute_threads: tessera_engine::default_compute_threads(),
        // The shipped defaults for the two flush triggers.
        flush_max_age_secs: 90,
        flush_max_items: 40_000,
        max_merged_segment_bytes: None,
        tier_width: None,
        segment_floor_bytes: None,
        coalesce_width: None,
        // No scheduled compaction.
        compaction: tessera_engine::CompactionSchedule::off(),
    }
}

/// [`spawn_server`], with the streamed viewport's flush threshold and write-stall budget chosen
/// by the caller.
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
        CorsOrigins::none(),
        stream_flush_bytes,
        stream_write_stall_ms,
        Arc::new(FaultSwitchboard::new()),
        DEFAULT_VISIBLE_WAIT_MAX_SECS,
        generous_bulk_gate(),
        |_| {},
    )
    .await
}

pub async fn spawn_server(bundle_root: &Path, cache_dir: &Path, wal_path: &Path) -> TestServer {
    spawn_server_with_config(bundle_root, cache_dir, wal_path, default_engine_config()).await
}

/// A server over the bundle at `dir/bundle`, keeping its cache and write-ahead log in `dir`.
pub async fn open(dir: impl AsRef<Path>) -> TestServer {
    open_with(dir, default_engine_config()).await
}

/// [`open`] under `config`.
pub async fn open_with(dir: impl AsRef<Path>, config: EngineConfig) -> TestServer {
    let dir = dir.as_ref();
    spawn_server_with_config(
        &dir.join("bundle"),
        &dir.join("cache"),
        &dir.join("wal.log"),
        config,
    )
    .await
}

/// Build the standard fixture ([`build_fixture`]) into `dir` and serve it.
pub async fn serve(dir: impl AsRef<Path>) -> TestServer {
    build_fixture(dir.as_ref(), N_ITEMS);
    open(dir).await
}

/// The standard fixture, served, under a `point_visibility` declaring `default` or none.
pub async fn serve_with_default(default: Option<&str>) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let access = AccessInput {
        source: tessera_build::config::AccessSource::Relation(tmp.path().join("pairs.parquet")),
        default: default.map(str::to_string),
    };
    build_fixture_with_access(tmp.path(), N_ITEMS, access);
    let server = open(&tmp).await;
    (tmp, server)
}

/// Stop `server` and serve the bundle in `dir` again, over the same cache and log.
pub async fn restart(server: TestServer, dir: impl AsRef<Path>) -> TestServer {
    server.shutdown().await;
    open(dir).await
}

/// A running server, a token for a principal holding both fixture terms, and the directory that
/// holds the bundle, cache and log, kept for as long as the server runs.
pub struct Served {
    pub server: TestServer,
    pub token: String,
    pub tmp: TempDir,
}

impl Served {
    /// Build a bundle into a fresh directory with `build` and serve it.
    pub async fn build<R>(build: impl FnOnce(&Path) -> R) -> Served {
        Served::build_with(build, default_engine_config()).await
    }

    /// [`Served::build`] under `config`.
    pub async fn build_with<R>(build: impl FnOnce(&Path) -> R, config: EngineConfig) -> Served {
        let tmp = TempDir::new().unwrap();
        build(tmp.path());
        Served::open_with(tmp, config).await
    }

    /// [`Served::build`] over a copy of the bundle, built once per test binary ([`copy_built`]).
    pub async fn copy<R>(
        once: &'static std::sync::OnceLock<TempDir>,
        build: impl FnOnce(&Path) -> R,
    ) -> Served {
        let tmp = TempDir::new().unwrap();
        copy_built(once, tmp.path(), build);
        Served::open(tmp).await
    }

    /// Serve the bundle already built in `tmp`.
    pub async fn open(tmp: TempDir) -> Served {
        Served::open_with(tmp, default_engine_config()).await
    }

    /// [`Served::open`] under `config`.
    pub async fn open_with(tmp: TempDir, config: EngineConfig) -> Served {
        let server = open_with(&tmp, config).await;
        let token = token_for(&server, &["0", "1"]).await;
        Served { server, token, tmp }
    }

    /// Take a fresh token for the same principal. The views a session may see are resolved when
    /// it authorises, so a test that creates a view and then reads it takes a new one first.
    pub async fn reauthorise(&mut self) {
        self.token = token_for(&self.server, &["0", "1"]).await;
    }

    /// Stop the server and serve the same bundle, cache and log again, with a fresh token.
    pub async fn restart(self) -> Served {
        self.restart_with(default_engine_config()).await
    }

    /// [`Served::restart`] under `config`.
    pub async fn restart_with(self, config: EngineConfig) -> Served {
        let Served { server, tmp, .. } = self;
        server.shutdown().await;
        Served::open_with(tmp, config).await
    }
}

/// The default test gate: generous enough that no test which issues a handful of sequential
/// requests can ever observe it. Only the gate-specific tests construct a deliberately tiny
/// [`ComputeGate`] to exercise shedding.
pub fn generous_test_gate() -> ComputeGate {
    ComputeGate::new(64, 64, 250)
}

/// The bulk-read lane every mount but the lane's own tests takes, generous for the same reason.
pub fn generous_bulk_gate() -> ComputeGate {
    ComputeGate::for_bulk_reads(16)
}

/// A server whose two admission gates and `[serve]` limits the caller chooses: the bulk-read tests
/// set the lane, the page ceilings, the response budgets and the stream budgets through `tune`.
pub async fn spawn_server_with_bulk_reads(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    compute_gate: ComputeGate,
    bulk_gate: ComputeGate,
    tune: impl FnOnce(&mut ServeLimits),
) -> TestServer {
    let config = default_engine_config();
    let max_k = config.max_k;
    let mut engine = Engine::open(bundle_root, cache_dir, wal_path, Passthrough::new(), config)
        .expect("engine should open against a freshly built bundle");
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    mount_server_with_flush(
        engine,
        max_k,
        compute_gate,
        generous_ingest_limits(),
        CorsOrigins::none(),
        1 << 20,
        10_000,
        Arc::new(FaultSwitchboard::new()),
        DEFAULT_VISIBLE_WAIT_MAX_SECS,
        bulk_gate,
        tune,
    )
    .await
}

/// [`spawn_server`] under `config`.
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

/// [`spawn_server_with_config`] behind `compute_gate`.
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

/// Start `engine`'s write executor and serve it on three listeners.
pub async fn spawn_server_from_engine(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
) -> TestServer {
    // A queue bound no test reaching this helper fills; the queue-full tests start their own.
    let mut engine = engine;
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    mount_server(engine, max_k, compute_gate).await
}

/// Serve `engine` as it is: its write executor started by the caller, or not started at all.
pub async fn mount_server(engine: Engine, max_k: usize, compute_gate: ComputeGate) -> TestServer {
    mount_server_with(
        engine,
        max_k,
        compute_gate,
        generous_ingest_limits(),
        CorsOrigins::none(),
    )
    .await
}

/// The control plane's bounds. `admission` bounds concurrent `/control/ingest` handlers and is
/// separate from the write executor's queue bound, which `start_write_executor` sets.
pub struct IngestLimits {
    pub admission: usize,
    pub max_batch_rows: usize,
    pub max_batch_bytes: usize,
    /// The buffered rows at which ingest is refused.
    pub buffer_max_items: usize,
    /// The artifact routes' bounds: the byte cap on `PUT` and `PATCH
    /// /control/layers/{name}/artifacts`, the artifacts per publication and the members per
    /// growth page.
    pub publish_max_body_bytes: usize,
    pub max_artifacts_per_request: usize,
    pub max_members_per_request: usize,
    /// The entities one artifact's `excluding` list may name.
    pub max_excluded_per_request: usize,
}

/// Bounds no test that is not about them reaches.
pub fn generous_ingest_limits() -> IngestLimits {
    IngestLimits {
        admission: 64,
        // Above the shipped 10 000: one test posts 40 000-row batches.
        max_batch_rows: 200_000,
        // Without a flush the buffer only grows, so this is far above what any test ingests.
        buffer_max_items: 10_000_000,
        max_batch_bytes: 64 * 1024 * 1024,
        publish_max_body_bytes: 64 * 1024 * 1024,
        max_artifacts_per_request: 100_000,
        max_members_per_request: 50_000_000,
        max_excluded_per_request: 1_000_000,
    }
}

/// As [`mount_server`], with the control-plane bounds chosen by the caller.
pub async fn mount_server_with_ingest_limits(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
) -> TestServer {
    mount_server_with(
        engine,
        max_k,
        compute_gate,
        ingest_limits,
        CorsOrigins::none(),
    )
    .await
}

/// `serve.visible_wait_max_secs`' shipped default.
pub const DEFAULT_VISIBLE_WAIT_MAX_SECS: u64 = 30;

/// The CORS settings, by name, so the production list cannot be passed where the development
/// list belongs.
#[derive(Default, Clone)]
pub struct CorsOrigins {
    /// `serve.dev_cors_origins`: the viewer and session planes.
    pub dev: Vec<String>,
    /// `serve.cors_origins`: the viewer plane only.
    pub production: Vec<String>,
    /// `serve.cors_loopback`: viewer plane only, and a rule instead of a list.
    pub loopback: bool,
}

impl CorsOrigins {
    /// Neither list set: no layer on any plane.
    pub fn none() -> Self {
        Self::default()
    }

    /// The development list alone.
    pub fn dev(origins: &[&str]) -> Self {
        Self {
            dev: origins.iter().map(|o| o.to_string()).collect(),
            ..Self::default()
        }
    }

    /// The production list alone.
    pub fn production(origins: &[&str]) -> Self {
        Self {
            production: origins.iter().map(|o| o.to_string()).collect(),
            ..Self::default()
        }
    }

    /// `serve.cors_loopback` alone: no list on either plane.
    pub fn loopback() -> Self {
        Self {
            loopback: true,
            ..Self::default()
        }
    }
}

/// [`spawn_server`] with `serve.visible_wait_max_secs`, how long a `wait=visible` answer is held,
/// chosen by the caller. `0` answers without waiting.
pub async fn spawn_server_with_visible_wait(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    visible_wait_max_secs: u64,
) -> TestServer {
    let config = default_engine_config();
    let max_k = config.max_k;
    let mut engine = Engine::open(bundle_root, cache_dir, wal_path, Passthrough::new(), config)
        .expect("engine should open against a freshly built bundle");
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once per engine");
    mount_server_with_flush(
        engine,
        max_k,
        generous_test_gate(),
        generous_ingest_limits(),
        CorsOrigins::none(),
        1 << 20,
        10_000,
        Arc::new(FaultSwitchboard::new()),
        visible_wait_max_secs,
        generous_bulk_gate(),
        |_| {},
    )
    .await
}

/// [`spawn_server`] with the CORS settings `cors`.
pub async fn spawn_server_with_cors(
    bundle_root: &Path,
    cache_dir: &Path,
    wal_path: &Path,
    cors: CorsOrigins,
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
        cors,
    )
    .await
}

/// [`mount_server`] with the ingest bounds and the CORS settings given.
async fn mount_server_with(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
    cors: CorsOrigins,
) -> TestServer {
    mount_server_with_flush(
        engine,
        max_k,
        compute_gate,
        ingest_limits,
        cors,
        1 << 20,
        10_000,
        Arc::new(FaultSwitchboard::new()),
        DEFAULT_VISIBLE_WAIT_MAX_SECS,
        generous_bulk_gate(),
        |_| {},
    )
    .await
}

/// [`mount_server`] for an engine whose executor was started with
/// `start_write_executor_with_faults(faults)`. Passing another board leaves `/control/faults/*`
/// arming switches nothing reads.
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
        CorsOrigins::none(),
        1 << 20,
        10_000,
        faults,
        DEFAULT_VISIBLE_WAIT_MAX_SECS,
        generous_bulk_gate(),
        |_| {},
    )
    .await
}

/// [`mount_server_with`], with every setting given and `tune` applied last over the limits.
#[allow(clippy::too_many_arguments)]
async fn mount_server_with_flush(
    engine: Engine,
    max_k: usize,
    compute_gate: ComputeGate,
    ingest_limits: IngestLimits,
    cors: CorsOrigins,
    stream_flush_bytes: usize,
    stream_write_stall_ms: u64,
    faults: Arc<FaultSwitchboard>,
    visible_wait_max_secs: u64,
    bulk_gate: ComputeGate,
    tune: impl FnOnce(&mut ServeLimits),
) -> TestServer {
    let mut limits = ServeLimits {
        max_k,
        // Small, so the fixtures' vocabularies arrive in pages.
        max_category_values: 4,
        // Small, so suggestions page and spend their walk budget on ordinary requests.
        max_suggestions: 4,
        max_suggestion_walk: 1_000,
        // Every test principal sees more than one entity, so suggestions always take the probe
        // route and no background sweep can race a test that reads `more`.
        max_suggest_set_entities: 1,
        ingest_max_batch_rows: ingest_limits.max_batch_rows,
        ingest_buffer_max_items: ingest_limits.buffer_max_items,
        ingest_max_batch_bytes: ingest_limits.max_batch_bytes,
        publish_max_body_bytes: ingest_limits.publish_max_body_bytes,
        max_artifacts_per_request: ingest_limits.max_artifacts_per_request,
        max_members_per_request: ingest_limits.max_members_per_request,
        max_excluded_per_request: ingest_limits.max_excluded_per_request,
        // On; the `bench-timing` feature still decides whether anything is sent.
        stage_timing: true,
        // At the 1 MiB default every fixture's points arrive in one frame; the tests of several
        // frames mount a smaller threshold.
        stream_flush_bytes,
        stream_write_stall_ms,
        dev_cors_origins: cors.dev,
        cors_origins: cors.production,
        cors_loopback: cors.loopback,
        visible_wait_max_secs,
        ..Default::default()
    };
    tune(&mut limits);
    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        heap: tessera_server::memory::HeapWatch::default(),
        limits,
        suggest_admission: tessera_server::state::SuggestAdmission::new(),
        compute_gate,
        bulk_gate,
        ingest_admission: IngestAdmission::new(ingest_limits.admission),
        session_credential: SESSION_CREDENTIAL.to_string(),
        operator_credential: OPERATOR_CREDENTIAL.to_string(),
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

    let serve_tasks = vec![
        tokio::spawn(async move {
            let _ = axum::serve(viewer_listener, viewer_router).await;
        }),
        tokio::spawn(async move {
            let _ = axum::serve(session_listener, session_router).await;
        }),
        tokio::spawn(async move {
            let _ = axum::serve(control_listener, control_router).await;
        }),
    ];

    TestServer {
        viewer_addr,
        session_addr,
        control_addr,
        client: reqwest::Client::new(),
        state,
        serve_tasks,
    }
}

/// Authorise a session for a principal holding `terms`, retrying while the admission gate sheds
/// under machine load. A test about shedding calls the route itself.
pub async fn authorise(server: &TestServer, terms: &[&str]) -> serde_json::Value {
    let auth_data = serde_json::json!({ "terms": terms }).to_string();
    let encoded = base64::engine::general_purpose::STANDARD.encode(auth_data);
    let resp = wait_for(
        "authorise",
        std::time::Duration::from_secs(60),
        async || {
            let resp = server
                .client
                .post(server.session_url("/session/authorise"))
                .bearer_auth(SESSION_CREDENTIAL)
                .json(&serde_json::json!({ "auth_data": encoded }))
                .send()
                .await
                .unwrap();
            if resp.status().as_u16() == 429 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                return None;
            }
            assert_eq!(resp.status(), 200, "authorise should succeed");
            Some(resp)
        },
    )
    .await;
    resp.json().await.unwrap()
}

/// The token [`authorise`] returns for a principal holding `terms`.
pub async fn token_for(server: &TestServer, terms: &[&str]) -> String {
    authorise(server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The runs of digits in `text`, in order: where a refusal names the caller's coordinates, these
/// are the coordinates and nothing else.
pub fn numbers_in(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect()
}

/// Whether `text` holds `token` with no letter or digit against either end, so `row 1` is not
/// found in `row 12`.
pub fn mentions(text: &str, token: &str) -> bool {
    text.match_indices(token).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + token.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// `bytes` in standard base64, the way the wire carries an external id.
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A built item's member address: its external id ([`external_id_of`]) in base64.
pub fn member(source_id: u64) -> String {
    b64(&external_id_of(source_id))
}

/// [`member`] for each of `range`.
pub fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

/// `PUT /control/layers` with `declaration`: the status and the body, or null where there is none.
pub async fn put_layer(
    server: &TestServer,
    declaration: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// Register the layer `declaration` names, failing the test unless it is created.
pub async fn register(server: &TestServer, declaration: serde_json::Value) {
    let (status, body) = put_layer(server, declaration).await;
    assert_eq!(status, 201, "the layer registers: {body}");
}

/// A flat layer `name` over `s0` whose artifacts list their members, with a closed value set, no
/// gate, and no content. A test changes a field by indexing the value.
pub fn flat_layer(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "title": name,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
}

/// One attempt of a wait: a value, or not yet. An attempt that returns `Err` says what it saw,
/// and the last of those is printed if the wait times out.
pub trait Attempt<T> {
    fn outcome(self) -> Result<T, String>;
}

impl<T> Attempt<T> for Option<T> {
    fn outcome(self) -> Result<T, String> {
        self.ok_or_else(String::new)
    }
}

impl<T> Attempt<T> for Result<T, String> {
    fn outcome(self) -> Result<T, String> {
        self
    }
}

impl Attempt<()> for bool {
    fn outcome(self) -> Result<(), String> {
        if self {
            Ok(())
        } else {
            Err(String::new())
        }
    }
}

/// Poll `attempt` every `every` until it gives a value, failing the test with the last thing it
/// saw if it has not within `within`.
pub async fn poll<T, A: Attempt<T>>(
    what: &str,
    within: std::time::Duration,
    every: std::time::Duration,
    mut attempt: impl AsyncFnMut() -> A,
) -> T {
    let deadline = std::time::Instant::now() + within;
    loop {
        let seen = match attempt().await.outcome() {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        if std::time::Instant::now() >= deadline {
            match seen.is_empty() {
                true => panic!("{what}: not within {within:?}"),
                false => panic!("{what}: not within {within:?}; last seen: {seen}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// [`poll`] every ten milliseconds.
pub async fn wait_for<T, A: Attempt<T>>(
    what: &str,
    within: std::time::Duration,
    attempt: impl AsyncFnMut() -> A,
) -> T {
    poll(what, within, std::time::Duration::from_millis(10), attempt).await
}

/// [`wait_for`] a condition.
pub async fn wait_until<A: Attempt<()>>(
    what: &str,
    within: std::time::Duration,
    done: impl AsyncFnMut() -> A,
) {
    wait_for(what, within, done).await
}

/// Send `request` until the answer is neither shed nor stale and return it, failing the test
/// unless a `200` arrives within a minute. A `429` is the admission gate shedding under machine
/// load and `x-tessera-stale: 1` a stale stamp; neither is the answer a test asks about.
pub async fn settled(request: impl AsyncFn() -> reqwest::Response) -> reqwest::Response {
    wait_for(
        "a settled answer",
        std::time::Duration::from_secs(60),
        async || {
            let resp = request().await;
            if resp.status().as_u16() == 429 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                return None;
            }
            assert_eq!(resp.status().as_u16(), 200, "the request answers");
            let stale = resp
                .headers()
                .get("x-tessera-stale")
                .is_some_and(|v| v == "1");
            if stale {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                return None;
            }
            Some(resp)
        },
    )
    .await
}

/// Poll the write executor's counters until `done` holds, failing the test after three minutes.
pub async fn wait_for_executor(
    server: &TestServer,
    what: &str,
    done: impl Fn(&tessera_engine::ExecutorStats) -> bool,
) {
    wait_until(what, std::time::Duration::from_secs(180), async || {
        let stats = server.state.engine.write_executor_stats();
        match done(&stats) {
            true => Ok(()),
            false => Err(format!("{stats:?}")),
        }
    })
    .await
}

/// Ask for a flush and wait until the publication it names has landed, so a test that writes on
/// the control plane can then read what a viewer is served. `POST /control/flush?wait=visible`
/// holds its answer for the server's visible wait; where that ends first (`visible: false`, a slow
/// publication under load), this keeps reading `/control/status` for up to two minutes more.
pub async fn tick(server: &TestServer) {
    let resp = server
        .client
        .post(server.control_url("/control/flush?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    if body["visible"] == serde_json::json!(true) {
        return;
    }
    let target = body["publication"]
        .as_u64()
        .unwrap_or_else(|| panic!("the 202 carries the publication number: {body}"));
    let what = format!("publication {target}");
    wait_until(&what, std::time::Duration::from_secs(120), async || {
        let publication = control_status(server).await["publication"].clone();
        match publication.as_u64() >= Some(target) {
            true => Ok(()),
            false => Err(format!("publication {publication}")),
        }
    })
    .await
}

/// Publish until nothing is buffered. A flush takes one view's rows, so a batch that landed in
/// several views needs a [`tick`] for each.
pub async fn drain(server: &TestServer) {
    wait_until(
        "the buffer drained",
        std::time::Duration::from_secs(120),
        async || {
            tick(server).await;
            let buffered = server.state.engine.buffered_items();
            if buffered == 0 {
                return Ok(());
            }
            let stats = server.state.engine.write_executor_stats();
            Err(format!(
                "{buffered} rows buffered, {} flushes, {} failures, {} flushable",
                stats.flushes, stats.flush_failures, stats.flushable_items
            ))
        },
    )
    .await
}

/// Ask for a fold and wait until it publishes, failing the test if any fold this server ran was
/// discarded.
pub async fn fold(server: &TestServer) {
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        202,
        "a fold is accepted at any time"
    );
    wait_for_executor(server, "the fold published", move |now| {
        assert_eq!(
            now.fold_failures, 0,
            "a fold was discarded rather than published"
        );
        now.folds > before.folds
    })
    .await;
}

/// Ingest one point, into `view` where the bundle holds several, then [`tick`] and [`fold`]. A
/// flush with nothing buffered publishes nothing, so the point gives the fold something to fold.
pub async fn flush_and_fold(server: &TestServer, view: Option<&str>) {
    let ingested = external_id_of(9_001);
    let mut request = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "flush-and-fold")
        .header("content-type", "application/vnd.apache.arrow.stream");
    if let Some(view) = view {
        request = request.header("x-tessera-view", view);
    }
    let resp = request
        .body(build_ingest_batch_optional(&[(
            Some(&ingested[..]),
            10.0,
            10.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    tick(server).await;
    fold(server).await;
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

/// One decoded `/v1/viewport` response body, every frame kind.
pub struct DecodedViewport {
    /// `(tile, visible, matched)` per row.
    pub tiles: Vec<TileRow>,
    pub served: Vec<u64>,
    /// `(tessera_id, code)` per point, concatenated across every kind-3 frame in order.
    pub points: Vec<PointRow>,
    /// `(cell, count)`, or `None` where no underlay was asked for.
    pub sub_cells: Option<Vec<(u64, u64)>>,
    /// The artifacts frame in the full projection. `None` where the body carried no artifacts
    /// frame or carried the identity projection, which is in
    /// [`DecodedViewport::artifacts_identity`]; `Some(vec![])` is a frame with no rows.
    pub artifacts: Option<Vec<ArtifactRow>>,
    /// The artifacts frame in the identity projection (`artifact_rows: "identity"`).
    pub artifacts_identity: Option<Vec<ArtifactIdentityRow>>,
    /// The trailer, parsed. The decoder asserts its key set.
    pub trailer: serde_json::Value,
    /// How many points frames the body carried.
    pub point_frames: usize,
    /// The body without the trailer, which carries timings: the bytes two equal answers share.
    pub deterministic_bytes: Vec<u8>,
}

/// One row of the artifacts frame in the full projection.
///
/// `PartialEq` and not `Eq`: a centroid is a mean and travels as `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactRow {
    pub layer: String,
    pub tessera_id: u64,
    pub key: Option<String>,
    /// How many members this principal can see, not how many the artifact has.
    pub masked_count: u64,
    /// Derived geometry, in grid units, computed over the members this principal can see. `None`
    /// is *the layer declares none* and never *withheld*.
    pub centroid: Option<[f64; 2]>,
    pub bbox: Option<[u32; 4]>,
    /// The artifact's drawn geometry as parts, then rings, then vertices. `None` where no served
    /// layer declares one or this row has none.
    pub shape: Option<Vec<Vec<Vec<[u32; 2]>>>>,
    /// The artifact's supplied content, one entry per kind its layer declares.
    pub content: Vec<String>,
    /// The rung this artifact is drawn at: its level on a levelled layer, its depth in this
    /// response on a treed one, 0 on a flat one.
    pub rung: u32,
    /// Whether a member this principal may see, inside the requested tiles, matched the request's
    /// filter. `None` where the request carried none.
    pub matched: Option<bool>,
    /// The same bit for the filter and the highlight together.
    pub highlighted: Option<bool>,
    /// The `tessera_id` of the artifact this row is attached to, a row of the same frame. `None`
    /// for an artifact attached to nothing.
    pub target: Option<u64>,
}

/// The identity projection's four columns (`artifact_rows: "identity"`), read back by a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentityRow {
    pub layer: String,
    pub tessera_id: u64,
    pub rung: u32,
    pub matched: Option<bool>,
    /// The same bit for the filter and the highlight together.
    pub highlighted: Option<bool>,
}

fn str_col(batch: &arrow::record_batch::RecordBatch, i: usize) -> arrow::array::StringArray {
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

/// Decode a complete streamed `/v1/viewport` body, failing the test unless the tiles frame is
/// first, one trailer is last and every frame is of a known kind.
pub fn decode_viewport_frames(bytes: &[u8]) -> DecodedViewport {
    let frames = tessera_wire::split_frames(bytes).expect("well-formed frame sequence");
    assert!(
        !frames.is_empty(),
        "a response carries at least tiles + trailer"
    );
    assert_eq!(
        frames.first().unwrap().0,
        tessera_wire::FRAME_TILES,
        "the tiles frame is first"
    );
    assert_eq!(
        frames.last().unwrap().0,
        tessera_wire::FRAME_TRAILER,
        "the trailer frame is last"
    );

    let mut tiles = Vec::new();
    let mut served = Vec::new();
    let mut points = Vec::new();
    let mut sub_cells: Option<Vec<(u64, u64)>> = None;
    let mut artifacts: Option<Vec<ArtifactRow>> = None;
    let mut artifacts_identity: Option<Vec<ArtifactIdentityRow>> = None;
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
                assert!(
                    artifacts.is_none() && artifacts_identity.is_none(),
                    "exactly one artifacts frame"
                );
                let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
                // The identity projection has five columns; the full one has more.
                let identity = reader.schema().fields().len() == 5;
                for batch in reader {
                    let batch = batch.unwrap();
                    // `layer` is dictionary-encoded in both projections (u16 keys over utf8).
                    let layer_at = |i: usize| -> String {
                        let column = batch
                            .column(0)
                            .as_any()
                            .downcast_ref::<arrow::array::DictionaryArray<
                                arrow::datatypes::UInt16Type,
                            >>()
                            .expect("`layer` is dictionary-encoded, u16 keys over utf8");
                        let values = column
                            .values()
                            .as_any()
                            .downcast_ref::<arrow::array::StringArray>()
                            .unwrap();
                        values
                            .value(column.key(i).expect("layer is never null"))
                            .to_string()
                    };
                    let tessera_id = u64_col(&batch, 1);
                    let u32_col_at = |col: usize, name: &str| {
                        batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::UInt32Array>()
                            .unwrap_or_else(|| panic!("`{name}` is a UInt32 at column {col}"))
                            .clone()
                    };
                    let bool_at = |col: usize, i: usize, name: &str| {
                        let column = batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::BooleanArray>()
                            .unwrap_or_else(|| {
                                panic!("`{name}` is a nullable Boolean at column {col}")
                            });
                        column.is_valid(i).then(|| column.value(i))
                    };
                    if identity {
                        // (layer, tessera_id, rung, matched, highlighted), by position.
                        let rows = artifacts_identity.get_or_insert_with(Vec::new);
                        let rung = u32_col_at(2, "rung");
                        for i in 0..batch.num_rows() {
                            rows.push(ArtifactIdentityRow {
                                layer: layer_at(i),
                                tessera_id: tessera_id.value(i),
                                rung: rung.value(i),
                                matched: bool_at(3, i, "matched"),
                                highlighted: bool_at(4, i, "highlighted"),
                            });
                        }
                        continue;
                    }
                    let rows = artifacts.get_or_insert_with(Vec::new);
                    let key = str_col(&batch, 2);
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
                    // The two shape columns follow the fixed columns, present only where a served
                    // layer declares a drawn geometry.
                    let shapes = batch.num_columns() > 16;
                    if shapes {
                        assert_eq!(
                            batch.num_columns(),
                            18,
                            "shape_x and shape_y travel together"
                        );
                        assert_eq!(batch.schema().field(16).name(), "shape_x");
                        assert_eq!(batch.schema().field(17).name(), "shape_y");
                    }
                    // One axis of the shape, as parts of rings.
                    let shape_axis = |col: usize, i: usize| {
                        let a = batch
                            .column(col)
                            .as_any()
                            .downcast_ref::<arrow::array::ListArray>()
                            .unwrap();
                        a.is_valid(i).then(|| {
                            let parts = a.value(i);
                            let parts = parts
                                .as_any()
                                .downcast_ref::<arrow::array::ListArray>()
                                .expect("shape_x/shape_y are a list of parts");
                            (0..parts.len())
                                .map(|p| {
                                    let rings = parts.value(p);
                                    let rings = rings
                                        .as_any()
                                        .downcast_ref::<arrow::array::ListArray>()
                                        .expect("a part is a list of rings");
                                    (0..rings.len())
                                        .map(|r| {
                                            let values = rings.value(r);
                                            let values = values
                                                .as_any()
                                                .downcast_ref::<arrow::array::UInt32Array>()
                                                .unwrap();
                                            (0..values.len())
                                                .map(|k| values.value(k))
                                                .collect::<Vec<_>>()
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .collect::<Vec<_>>()
                        })
                    };
                    for i in 0..batch.num_rows() {
                        let shape = if !shapes {
                            None
                        } else {
                            match (shape_axis(16, i), shape_axis(17, i)) {
                                (Some(xs), Some(ys)) => {
                                    assert_eq!(xs.len(), ys.len(), "the axes disagree on parts");
                                    Some(
                                        xs.into_iter()
                                            .zip(ys)
                                            .map(|(px, py)| {
                                                assert_eq!(px.len(), py.len(), "rings differ");
                                                px.into_iter()
                                                    .zip(py)
                                                    .map(|(rx, ry)| {
                                                        assert_eq!(rx.len(), ry.len());
                                                        rx.into_iter()
                                                            .zip(ry)
                                                            .map(|(x, y)| [x, y])
                                                            .collect::<Vec<_>>()
                                                    })
                                                    .collect::<Vec<_>>()
                                            })
                                            .collect(),
                                    )
                                }
                                (None, None) => None,
                                _ => panic!("a shape with one axis and not the other"),
                            }
                        };
                        rows.push(ArtifactRow {
                            layer: layer_at(i),
                            tessera_id: tessera_id.value(i),
                            key: key.is_valid(i).then(|| key.value(i).to_string()),
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
                            shape,
                            content: {
                                let a = batch
                                    .column(10)
                                    .as_any()
                                    .downcast_ref::<arrow::array::ListArray>()
                                    .expect("content is a list of utf8");
                                let values = a.value(i);
                                let values = values
                                    .as_any()
                                    .downcast_ref::<arrow::array::StringArray>()
                                    .expect("a content is utf8");
                                (0..values.len())
                                    .map(|k| values.value(k).to_string())
                                    .collect()
                            },
                            // Read by position, so a column inserted ahead fails the test.
                            rung: u32_col_at(12, "rung").value(i),
                            // Null where the request carried no filter.
                            matched: bool_at(13, i, "matched"),
                            highlighted: bool_at(14, i, "highlighted"),
                            target: {
                                let a = batch
                                    .column(15)
                                    .as_any()
                                    .downcast_ref::<arrow::array::UInt64Array>()
                                    .expect("`target` is a nullable UInt64");
                                a.is_valid(i).then(|| a.value(i))
                            },
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
                // `stage_ns` is the one optional key.
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
    // The trailer's counts agree with the body.
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
    // The tiles' `served` counts partition the points.
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
        artifacts_identity,
        trailer,
        point_frames,
        deterministic_bytes: bytes[..deterministic_end].to_vec(),
    }
}

/// A viewport body's tiles as `(tile, visible, matched)` and points as `(tessera_id, code)`.
pub fn decode_viewport(bytes: &[u8]) -> (Vec<TileRow>, Vec<PointRow>) {
    let decoded = decode_viewport_frames(bytes);
    (decoded.tiles, decoded.points)
}

/// `GET /control/status`'s body.
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

/// An ingest batch's `access` column: `rows` gives each row's labels, and an empty slice is a row
/// with none. Declare the field with [`access_field`].
pub fn access_lists(rows: &[&[&str]]) -> arrow::array::ListArray {
    let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
    for labels in rows {
        for label in *labels {
            builder.values().append_value(label);
        }
        builder.append(true);
    }
    builder.finish()
}

/// The `access` field for a schema, carrying the list's own element field.
pub fn access_field(access: &arrow::array::ListArray) -> Field {
    Field::new("access", access.data_type().clone(), false)
}

/// [`access_lists`] for the common case of exactly one label per row.
pub fn access_column<'a>(labels: impl IntoIterator<Item = &'a str>) -> arrow::array::ListArray {
    let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
    for label in labels {
        builder.values().append_value(label);
        builder.append(true);
    }
    builder.finish()
}

/// An Arrow ingest batch of `(external_id, x, y, access label)` rows, where a row may carry no
/// external id.
pub fn build_ingest_batch_optional(rows: &[(Option<&[u8]>, f32, f32, &str)]) -> Vec<u8> {
    let access_array = access_column(rows.iter().map(|(_, _, _, a)| *a));
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access_array),
    ]));
    let ext_array = BinaryArray::from_iter(rows.iter().map(|(id, _, _, _)| *id));
    let x_array = Float32Array::from_iter_values(rows.iter().map(|(_, x, _, _)| *x));
    let y_array = Float32Array::from_iter_values(rows.iter().map(|(_, _, y, _)| *y));

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

/// One decoded `POST /v1/items` body.
pub struct DecodedItems {
    /// The kind-6 head.
    pub head: serde_json::Value,
    /// Each kind-7 records frame's batch, with the kind-8 page end that follows it.
    pub pages: Vec<(RecordBatch, serde_json::Value)>,
    /// The kind-4 trailer.
    pub trailer: serde_json::Value,
    /// Each records frame's payload as sent, for a test about its encoding.
    pub records_payloads: Vec<Vec<u8>>,
}

impl DecodedItems {
    /// Every page's `tessera_id` column, in order.
    pub fn tessera_ids(&self) -> Vec<u64> {
        self.pages
            .iter()
            .flat_map(|(batch, _)| {
                batch
                    .column_by_name("tessera_id")
                    .expect("every page leads with tessera_id")
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("tessera_id is uint64")
                    .values()
                    .to_vec()
            })
            .collect()
    }
}

/// Decodes an items body strictly: one head first, a page end after every records frame and
/// nowhere else, one trailer last, and one batch in every records frame.
pub fn decode_items(bytes: &[u8]) -> DecodedItems {
    use tessera_wire::{FRAME_ITEMS_HEAD, FRAME_PAGE_END, FRAME_RECORDS, FRAME_TRAILER};
    let frames = tessera_wire::split_frames(bytes).expect("an items body splits into frames");
    assert!(frames.len() >= 2, "a head and a trailer at least");
    let json = |payload: &[u8]| -> serde_json::Value {
        serde_json::from_slice(payload).expect("a JSON frame parses")
    };
    let (first_kind, first) = frames[0];
    assert_eq!(first_kind, FRAME_ITEMS_HEAD, "the head is first");
    let (last_kind, last) = frames[frames.len() - 1];
    assert_eq!(last_kind, FRAME_TRAILER, "the trailer is last");
    let mut pages = Vec::new();
    let mut records_payloads = Vec::new();
    let middle = &frames[1..frames.len() - 1];
    assert!(middle.len().is_multiple_of(2), "every records frame has its page end");
    for pair in middle.chunks(2) {
        assert_eq!(pair[0].0, FRAME_RECORDS, "a records frame, then its page end");
        assert_eq!(pair[1].0, FRAME_PAGE_END, "a records frame, then its page end");
        let mut batches: Vec<RecordBatch> = StreamReader::try_new(Cursor::new(pair[0].1), None)
            .expect("a records frame is an Arrow stream")
            .map(|batch| batch.expect("a records batch decodes"))
            .collect();
        assert_eq!(batches.len(), 1, "one batch per records frame");
        pages.push((batches.remove(0), json(pair[1].1)));
        records_payloads.push(pair[0].1.to_vec());
    }
    DecodedItems {
        head: json(first),
        pages,
        trailer: json(last),
        records_payloads,
    }
}
