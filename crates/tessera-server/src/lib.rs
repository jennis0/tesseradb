//! `tessera-server`: the viewer, session and control planes, and the `tessera serve` entry point.
//!
//! [`prepare`] does everything that can fail before a listener is bound: it loads the config and
//! opens the engine (bundle verification, WAL replay, plugin load). [`run`] binds the three
//! planes, announces their addresses on stdout and serves. A refusal to start is therefore
//! testable without a socket.

pub mod control;
pub mod cors;
mod decode;
pub mod error;
mod filter_dto;
pub mod health;
pub mod memory;
mod records;
pub mod session;
pub mod state;
mod stream;
pub mod viewer;

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

use tessera_config::{Config, ControlListen};
use state::{AppState, ComputeGate, SessionRegistry};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Everything [`prepare`] built, for [`run`] to bind and serve.
pub struct Prepared {
    pub state: Arc<AppState>,
    pub config: Config,
}

/// Loads the config and opens the engine. A missing `[disclosure]` section, an unreadable bundle
/// or a WAL that fails its CRC check returns `Err` here, before any socket is bound.
pub fn prepare(config_path: &Path) -> Result<Prepared, BoxError> {
    let config = tessera_config::load(config_path)?;
    // Before the engine opens, and so before any thread that makes an arena exists: the cap
    // bounds arena creation and does nothing about arenas already made.
    memory::cap_arenas(config.compute_threads);

    // Read here rather than at parse, since a build reads the same file and needs no credential,
    // and before the bundle opens, so a missing credential refuses before the WAL is touched.
    let session_credential = config.session_credential.resolve("session")?;
    let operator_credential = config.operator_credential.resolve("operator")?;

    // `[serve]` is optional because a build reads this file too, but a server needs all three
    // addresses and gets no default. Port 0 is allowed; the announce line reports the real port.
    for (what, declared, example) in [
        ("viewer", config.viewer_addr.is_some(), "127.0.0.1:8080"),
        ("session", config.session_addr.is_some(), "127.0.0.1:8081"),
        ("control", config.control_listen.is_some(), "unix:/run/tessera/control.sock"),
    ] {
        if !declared {
            return Err(format!(
                "this deployment declares no `{what}` address; add one under `[serve]`, such as \
                 `{what} = \"{example}\"`"
            )
            .into());
        }
    }

    let engine_config = EngineConfig {
        token_max_lifetime_secs: config.token_max_lifetime_secs,
        max_k: config.max_k,
        k_min: config.k_min,
        k_max_marks: config.k_max_marks,
        theta_target_marks: config.theta_target_marks,
        max_underlay_offset: config.max_underlay_offset,
        max_underlay_cells: config.max_underlay_cells,
        max_tiles_per_request: config.max_tiles_per_request,
        compute_threads: config.compute_threads,
        flush_max_age_secs: config.flush_max_age_secs,
        flush_max_items: config.flush_max_items,
        max_merged_segment_bytes: config.max_merged_segment_bytes,
        tier_width: Some(config.tier_width),
        segment_floor_bytes: Some(config.segment_floor_bytes),
        coalesce_width: Some(config.coalesce_width),
        // The engine's own default is off, since a fold is minutes to hours of IO; this is where
        // the deployment's setting is applied.
        compaction: config.compaction,
    };
    let mut engine = Engine::open(
        &config.bundle_path,
        &config.cache_dir,
        &config.wal_path,
        Passthrough::new(),
        engine_config,
    )?;
    // Started here rather than in `Engine::open`, so an engine that never ingests starts no
    // thread. The engine owns the handle; `/control/*` reaches it through `state.engine`.
    #[cfg(not(feature = "fault-injection"))]
    engine.start_write_executor(config.ingest_queue_bound)?;
    // The executor starts with every fault site disarmed; `AppState` keeps the switchboard so the
    // control plane can arm one.
    #[cfg(feature = "fault-injection")]
    let faults = {
        let faults = Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new());
        engine.start_write_executor_with_faults(config.ingest_queue_bound, Arc::clone(&faults))?;
        faults
    };
    engine.set_cache_bounds(
        config.row_projection_cache_bytes,
        config.fragment_cache_bytes,
    );
    // A separate setter, since only a corpus with a row-major layer needs this cache.
    engine.set_masked_count_cache_bytes(config.masked_count_cache_bytes);
    engine.set_occupancy_cache_bytes(config.occupancy_cache_bytes);
    // The region leaf's cell budget, and the bound on decompositions cached across principals.
    engine.set_max_region_cells(config.max_region_cells);
    engine.set_region_cache_bytes(config.region_cache_bytes);
    // How long a request waits on another request's row-projection build before it is shed.
    engine.set_single_flight_wait_ms(config.single_flight_wait_ms);
    // Crossing the limit raises a counter and a log line and nothing else. Set after replay, so a
    // node that replayed a WAL already over the limit alarms at startup.
    engine.set_overlay_soft_limit(config.overlay_soft_limit);
    engine.set_commit_window_max_rows(config.commit_window_max_items);
    let engine = engine;

    // Built here so a runtime that cannot be built fails the start rather than the first
    // suppression. `/control/changes` never shares tokio's blocking pool.
    control::init_deny_runtime()?;

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        // The baseline is taken with the bundle open and the caches empty, so the first trim
        // answers serving growth rather than the open.
        heap: crate::memory::HeapWatch::default(),
        limits: state::ServeLimits::from_config(&config),
        suggest_admission: state::SuggestAdmission::new(),
        // Admits the viewport, item and artifact routes and `/session/authorise`; never the
        // control plane or the health probes.
        compute_gate: ComputeGate::new(
            config.compute_admission,
            config.compute_queue,
            config.admission_timeout_ms,
        ),
        // `POST /v1/items` only.
        bulk_gate: ComputeGate::for_bulk_reads(config.bulk_admission),
        // The viewer gate never covers the control plane, so writes have a limiter of their own.
        ingest_admission: state::IngestAdmission::new(config.ingest_admission),
        session_credential,
        operator_credential,
        #[cfg(feature = "fault-injection")]
        faults,
    });

    // The server knows no memory cap to hold bulk reads against, so their figure is logged for
    // the operator to compare with the cap the deployment runs under: each read builds a page in
    // about four page sizes and holds three encoded pages, two queued for the body and one being
    // written.
    tracing::info!(
        bulk_admission = config.bulk_admission,
        max_page_bytes = config.max_page_bytes,
        bulk_read_memory_bytes = tessera_config::bulk_read_memory_bytes(&config),
        "bulk reads may hold this much memory at once, within the process's memory cap"
    );

    // At `warn`, because this lets a page from another origin present the session credential.
    // `serve.cors_origins` names pages that may present tokens and gets no warning.
    if !config.dev_cors_origins.is_empty() {
        tracing::warn!(
            origins = ?config.dev_cors_origins,
            "serve.dev_cors_origins is set: these browser origins may present session tokens and \
             the session credential to this process. This is a DEVELOPMENT affordance — do not \
             enable it in a deployment."
        );
    }

    // At `info`: a loopback page may present only a token, never the credential that mints tokens.
    if config.cors_loopback {
        tracing::info!(
            "serve.cors_loopback is set: a page served from localhost, 127.0.0.1 or [::1], on any \
             port, may present a token to the viewer plane. The session and control planes are \
             unaffected."
        );
    }

    Ok(Prepared { state, config })
}

/// The one line `run` writes to stdout once all three planes are listening. A declared port 0
/// is chosen by the kernel, so only the process can report the address. Field order is key order.
#[derive(serde::Serialize)]
struct Listening<'a> {
    event: &'a str,
    viewer: String,
    session: String,
    control: String,
}

/// Binds all three listeners and serves until one of them errors. The control plane binds a
/// unix socket unless configured as loopback TCP.
pub async fn run(prepared: Prepared) -> Result<(), BoxError> {
    serve_announcing(prepared, std::io::stdout()).await
}

/// [`run`], with the announce line written somewhere a test can read. Stdout carries only that
/// line, a JSON object written and flushed once every plane is bound and every router built, so
/// a supervisor that reads it can send a request at once. Diagnostics go to stderr.
pub async fn serve_announcing<W: std::io::Write>(
    prepared: Prepared,
    mut announce_to: W,
) -> Result<(), BoxError> {
    let Prepared { state, config } = prepared;

    let viewer_addr = config
        .viewer_addr
        .expect("prepare() refuses a serve with no viewer address");
    let session_addr = config
        .session_addr
        .expect("prepare() refuses a serve with no session address");
    let viewer_listener = tokio::net::TcpListener::bind(viewer_addr).await?;
    let session_listener = tokio::net::TcpListener::bind(session_addr).await?;

    // Bound before the announce, so the line names three live planes.
    enum ControlBound {
        Tcp(tokio::net::TcpListener),
        Unix(tokio::net::UnixListener, std::path::PathBuf),
    }
    let control_bound = match config
        .control_listen
        .expect("prepare() refuses a serve with no control address")
    {
        ControlListen::Tcp(addr) => ControlBound::Tcp(tokio::net::TcpListener::bind(addr).await?),
        ControlListen::Unix(path) => {
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            ControlBound::Unix(listener, path)
        }
    };

    let viewer_router = viewer::router(Arc::clone(&state));
    let session_router = session::router(Arc::clone(&state));
    let control_router = control::router(Arc::clone(&state));

    let listening = Listening {
        event: "listening",
        viewer: viewer_listener.local_addr()?.to_string(),
        session: session_listener.local_addr()?.to_string(),
        control: match &control_bound {
            ControlBound::Tcp(listener) => listener.local_addr()?.to_string(),
            ControlBound::Unix(_, path) => format!("unix:{}", path.display()),
        },
    };
    writeln!(announce_to, "{}", serde_json::to_string(&listening)?)?;
    announce_to.flush()?;

    let viewer_task =
        tokio::spawn(async move { axum::serve(viewer_listener, viewer_router).await });
    let session_task =
        tokio::spawn(async move { axum::serve(session_listener, session_router).await });
    let control_task = match control_bound {
        ControlBound::Tcp(listener) => {
            tokio::spawn(async move { axum::serve(listener, control_router).await })
        }
        ControlBound::Unix(listener, _) => {
            tokio::spawn(async move { axum::serve(listener, control_router).await })
        }
    };

    let (v, s, c) = tokio::try_join!(viewer_task, session_task, control_task)?;
    v?;
    s?;
    c?;
    Ok(())
}
