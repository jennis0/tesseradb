//! `tessera-server` — the three HTTP planes (viewer/session/control), fail-closed config, and the
//! `tessera serve` entry point (task-13 brief).
//!
//! [`prepare`] does everything that can fail *before* any listener is bound: load `tessera.toml`
//! (fail-closed on a missing `[disclosure]` section — design §7.5/§2.3), open the engine (bundle
//! digest verification, WAL replay, plugin load). [`run`] takes the result and binds/serves the
//! three planes forever. Splitting the two means "the process refuses to start" (test (h)) is
//! observable without ever attempting to listen on a socket.

pub mod config;
pub mod control;
pub mod error;
pub mod health;
pub mod session;
pub mod state;
pub mod viewer;

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

use config::{Config, ControlListen};
use state::{AppState, ComputeGate, SessionRegistry};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Everything [`prepare`] built: the shared state and the resolved config it needs to bind
/// listeners in [`run`].
pub struct Prepared {
    pub state: Arc<AppState>,
    pub config: Config,
}

/// Load config and open the engine. Fails closed: a missing `[disclosure]` section, an
/// unreadable bundle, or a WAL that fails the positional CRC rule all return `Err` here, before
/// any socket is ever bound.
pub fn prepare(config_path: &Path) -> Result<Prepared, BoxError> {
    let config = config::load(config_path)?;

    let engine_config = EngineConfig {
        token_max_lifetime_secs: config.token_max_lifetime_secs,
        max_k: config.max_k,
        k_min: config.k_min,
        k_max_marks: config.k_max_marks,
        theta_target_marks: config.theta_target_marks,
        max_underlay_offset: config.max_underlay_offset,
        max_underlay_cells: config.max_underlay_cells,
        max_tiles_per_request: config.max_tiles_per_request,
        // D-D: the same knob D-B validated at parse time (`serve.compute_threads`, refused at
        // `0` — `ConfigError::ComputeThreadsZero`) sizes `Engine::open`'s shared rayon pool —
        // the parallel-sweep CPU bound. Distinct from `compute_admission` (below), which bounds
        // in-flight *requests*, not CPU, and defaults to a multiple of this number (D-B retune,
        // `COMPUTE_ADMISSION_MULTIPLIER`): admitted requests may now oversubscribe this pool
        // during their serialise phase, deliberately, because small requests are latency-bound on
        // scheduling rather than CPU.
        compute_threads: config.compute_threads,
        // Lifecycle §2.2's two pin bounds, landed as config keys by the seam (Task 0b) and given
        // their consumer's constructor by the Task 0 gate (F3): `Engine::open` hands both to
        // `PinManager::new`, which holds them until Task 4 builds the drain list they bound.
        // Wired now rather than at Task 4 because Track C owns `pins.rs` and this file is
        // `[shared]` — a knob that reaches its consumer only via a later track's edit to a shared
        // construction site is a knob that quietly does nothing in the meantime.
        pin_ttl_secs: config.pin_ttl_secs,
        pins_per_session_max: config.pins_per_session_max,
    };
    let mut engine = Engine::open(
        &config.bundle_path,
        &config.cache_dir,
        &config.wal_path,
        Passthrough::new(),
        engine_config,
    )?;
    // Phase 2 stage 2.1, Task 3a: move the WAL onto its own thread and open the two write queues.
    // Started here rather than inside `Engine::open` so that an engine which never ingests — every
    // read-only test, bench, example and embedder — starts no thread at all; see
    // `Engine::start_write_executor` for why the config-field and open-parameter routes are closed
    // by the stage's frozen files. `ingest_queue_bound` was landed by the seam (`config.rs`) and
    // this is its only consumer; Task 6 adds the startup headroom assertion over it.
    //
    // Nothing is stored in `AppState`: the engine owns the handle, so `/control/*` reaches the
    // executor through `state.engine` exactly as it reached the WAL before.
    engine.start_write_executor(config.ingest_queue_bound)?;
    let engine = engine;

    // Phase 2 stage 2.1, Task 3b: the deny lane's own blocking runtime, built here so a runtime
    // that cannot be constructed is a fail-to-start rather than a panic discovered by the first
    // suppression — the same rule Task 3a applied when it replaced the executor's spawn `expect`
    // with a typed `ExecutorStartError::Spawn`. See `control::init_deny_runtime` for why
    // `/control/changes` does not share tokio's blocking pool at all.
    control::init_deny_runtime()?;

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        max_k: config.max_k,
        // D-B: gates only /v1/viewport, /v1/items and /session/authorise (each handler wraps its
        // own closure); never the control plane, never /healthz/readyz/meta/revoke (D13).
        compute_gate: ComputeGate::new(
            config.compute_admission,
            config.compute_queue,
            config.admission_timeout_ms,
        ),
        stage_timing: config.stage_timing,
        min_visible_members: config.min_visible_members,
        session_credential: config.session_credential.clone(),
        operator_credential: config.operator_credential.clone(),
    });

    Ok(Prepared { state, config })
}

/// Bind all three listeners and serve forever (or until one of them errors). The viewer and
/// session planes always bind TCP; the control plane binds a unix socket unless configured as
/// loopback TCP (tests, and the documented Windows shape — SA §4.2).
pub async fn run(prepared: Prepared) -> Result<(), BoxError> {
    let Prepared { state, config } = prepared;

    let viewer_listener = tokio::net::TcpListener::bind(config.viewer_addr).await?;
    let session_listener = tokio::net::TcpListener::bind(config.session_addr).await?;

    let viewer_router = viewer::router(Arc::clone(&state));
    let session_router = session::router(Arc::clone(&state));
    let control_router = control::router(Arc::clone(&state));

    let viewer_task =
        tokio::spawn(async move { axum::serve(viewer_listener, viewer_router).await });
    let session_task =
        tokio::spawn(async move { axum::serve(session_listener, session_router).await });

    let control_task = match config.control_listen {
        ControlListen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tokio::spawn(async move { axum::serve(listener, control_router).await })
        }
        ControlListen::Unix(path) => {
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            tokio::spawn(async move { axum::serve(listener, control_router).await })
        }
    };

    let (v, s, c) = tokio::try_join!(viewer_task, session_task, control_task)?;
    v?;
    s?;
    c?;
    Ok(())
}
