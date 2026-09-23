//! `tessera-server` — the three HTTP planes (viewer/session/control) and the `tessera serve` entry
//! point.
//!
//! [`prepare`] does everything that can fail *before* any listener is bound: load `tessera.toml`
//! (fail-closed on a missing `[disclosure]` section — design §7.5/§2.3), open the engine (bundle
//! digest verification, WAL replay, plugin load). [`run`] takes the result, binds the three
//! planes, announces the bound addresses on stdout and serves them forever. Splitting the two
//! means "the process refuses to start" (test (h)) is observable without ever attempting to
//! listen on a socket.

pub mod control;
pub mod cors;
mod decode;
pub mod error;
mod filter_dto;
pub mod health;
pub mod memory;
pub mod session;
pub mod state;
pub mod viewer;

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

use tessera_config::{Config, ControlListen};
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
    let config = tessera_config::load(config_path)?;
    // **Before the engine opens**, which is before the compute pool, the reactor and the write
    // executor exist: the cap bounds arena creation and does nothing about arenas already made.
    // See `memory::arena_max` for the width it takes and what capping costs.
    memory::cap_arenas(config.compute_threads);

    // **The two serving secrets, read before anything is opened.** They are located in
    // `tessera.toml` and read here rather than at parse, because `tessera build` reads the same
    // file and has no business requiring a serving credential to be exported before it will write
    // a bundle (`configuration.md` §3). Here means *first*, though: a plane that cannot be
    // credentialed must refuse before the bundle is opened and the WAL is touched, not after.
    let session_credential = config.session_credential.resolve("session")?;
    let operator_credential = config.operator_credential.resolve("operator")?;

    // **The addresses, for the same reason and with the same posture.** `[serve]` is optional in
    // the deployment file because `tessera build` reads it too and a build has nothing to listen
    // on; what is not optional is a *server* coming up without them. Refused here rather than
    // defaulted, on SA §7's rule — a default port is a listening socket nobody chose. A declared
    // TCP address may name port 0, which is a port the caller asked the kernel to choose and then
    // reads back from the announce line `run` writes; it is a stated address, not a default.
    for (what, declared) in [
        ("viewer", config.viewer_addr.is_some()),
        ("session", config.session_addr.is_some()),
        ("control", config.control_listen.is_some()),
    ] {
        if !declared {
            return Err(format!(
                "this deployment declares no `{what}` address. `tessera serve` needs all three — \
                 add them under `[serve]` in the deployment file:\n\n    [serve]\n    \
                 viewer  = \"127.0.0.1:8080\"\n    session = \"127.0.0.1:8081\"\n    \
                 control = \"unix:/run/tessera/control.sock\"\n\n`[serve]` is optional because \
                 `tessera build` reads this same file and has nothing to listen on; it is required \
                 to serve, and there is no default because a default port is a socket nobody chose"
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
        // Compaction §9's automatic trigger. The engine's own default is `off` — a fold is minutes
        // to hours of IO and a library type may not start one from a default nobody chose — so
        // this is the one place §9's defaults are applied, which is also the one place an operator
        // can see and change them.
        compaction: config.compaction,
    };
    let mut engine = Engine::open(
        &config.bundle_path,
        &config.cache_dir,
        &config.wal_path,
        Passthrough::new(),
        engine_config,
    )?;
    // Move the WAL onto its own thread and open the two write queues. Started here rather than
    // inside `Engine::open` so that an engine which never ingests — every read-only test, bench,
    // example and embedder — starts no thread at all. This is `ingest_queue_bound`'s only consumer.
    //
    // Nothing is stored in `AppState`: the engine owns the handle, so `/control/*` reaches the
    // executor through `state.engine` exactly as it reached the WAL before.
    #[cfg(not(feature = "fault-injection"))]
    engine.start_write_executor(config.ingest_queue_bound)?;
    // The faults build (decision 0071): the executor starts with a switchboard, disarmed — every
    // site is a no-op until `/control/faults/arm` names one — and `AppState` keeps the other end
    // so the control plane arms the thread that pauses.
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
    // The third bound, and its own setter for the reason `Engine::set_masked_count_cache_bytes`
    // gives: it exists for a deployment that has a row-major layer at all, which is a property of
    // the corpus rather than of the box.
    engine.set_masked_count_cache_bytes(config.masked_count_cache_bytes);
    engine.set_occupancy_cache_bytes(config.occupancy_cache_bytes);
    // The region leaf's two knobs (selection-operand §2, §6): the cell budget the descent stops
    // at, and the bound on the decompositions held across principals.
    engine.set_max_region_cells(config.max_region_cells);
    engine.set_region_cache_bytes(config.region_cache_bytes);
    // `single_flight_wait_ms`' consumer — how long a request parks on another request's
    // row-projection build before it is shed (decision 0058).
    engine.set_single_flight_wait_ms(config.single_flight_wait_ms);
    // `overlay_soft_limit`'s consumer. **It alarms; it does not act.**
    // ⊘ Specified, not implemented: the compaction fold that would bring an over-limit overlay back
    // down does not exist, so crossing the limit raises a counter and a log line and nothing else —
    // an operator who sees the alarm has to act on it. Set after replay, and the setter evaluates
    // the predicate once as it lands, so a node that replayed a WAL already over the limit alarms
    // at startup rather than waiting for the next deny.
    engine.set_overlay_soft_limit(config.overlay_soft_limit);
    // `commit_window_max_items`' consumer — the row count at which a commit window closes, and with
    // it the scope of design §11.1's signature sort.
    engine.set_commit_window_max_rows(config.commit_window_max_items);
    let engine = engine;

    // The deny lane's own blocking runtime, built here so a runtime that cannot be constructed is a
    // fail-to-start rather than a panic discovered by the first suppression — the same rule that
    // makes the write executor's spawn failure a typed `ExecutorStartError::Spawn`. See
    // `control::init_deny_runtime` for why `/control/changes` does not share tokio's blocking pool
    // at all.
    control::init_deny_runtime()?;

    let state = Arc::new(AppState {
        engine,
        sessions: Mutex::new(SessionRegistry::default()),
        // Its baseline is the anonymous set as it stands here: the bundle is open and the caches
        // are empty, so the first trim answers serving growth rather than the open.
        heap: crate::memory::HeapWatch::default(),
        limits: state::ServeLimits::from_config(&config),
        suggest_admission: state::SuggestAdmission::new(),
        // Gates only /v1/viewport, /v1/items and /session/authorise (each handler wraps its own
        // closure); never the control plane, and never /healthz, /readyz, /meta or /revoke — the
        // probes are deliberately off the control plane and outside every gate
        // (docs/decisions/0011-health-probes-off-control-plane.md).
        compute_gate: ComputeGate::new(
            config.compute_admission,
            config.compute_queue,
            config.admission_timeout_ms,
        ),
        // The control plane's own bounds, deliberately separate from `compute_gate`: the viewer
        // gate never covers the control plane, so writes need a limiter of their own.
        ingest_admission: state::IngestAdmission::new(config.ingest_admission),
        session_credential,
        operator_credential,
        #[cfg(feature = "fault-injection")]
        faults,
    });

    // Loud, and at `warn`, because the key's effect is to let a page from another origin present a
    // session token and the session credential to this process. It is a development affordance;
    // T2 (server-mediated, with verified assertions) remains the documented integration topology —
    // client-interaction §7 tabulates the four topologies and names the two anti-patterns.
    //
    // `serve.cors_origins` gets no warning of its own. It is a deployment's deliberate statement
    // about which pages may present its tokens, not a seam left open by accident, and a warning
    // on every start would train an operator to read this one past as well (decision 0102).
    if !config.dev_cors_origins.is_empty() {
        tracing::warn!(
            origins = ?config.dev_cors_origins,
            "serve.dev_cors_origins is set: these browser origins may present session tokens and \
             the session credential to this process. This is a DEVELOPMENT affordance — do not \
             enable it in a deployment."
        );
    }

    // At `info`, and once. `serve.cors_loopback` is a disclosure control, so an operator reading
    // the log should see that it is on. It sits below `dev_cors_origins` because what a loopback
    // page may present is a token, which is per-principal, already scoped and already expiring,
    // and never the credential that mints tokens. A `warn` would put the two at one level and
    // teach a reader to pass both by (decision 0102).
    if config.cors_loopback {
        tracing::info!(
            "serve.cors_loopback is set: a page served from localhost, 127.0.0.1 or [::1], on any \
             port, may present a token to the viewer plane. The session and control planes are \
             unaffected."
        );
    }

    Ok(Prepared { state, config })
}

/// The one line `run` writes to stdout once all three planes are listening: where each plane
/// ended up. A TCP address a deployment declares with port 0 is a kernel-chosen port, so the
/// address a supervisor needs exists only after the bind, and only the process can report it.
///
/// The field order is the line's key order, which is why this is a struct and not a map.
#[derive(serde::Serialize)]
struct Listening<'a> {
    event: &'a str,
    viewer: String,
    session: String,
    control: String,
}

/// Bind all three listeners and serve forever (or until one of them errors). The viewer and
/// session planes always bind TCP; the control plane binds a unix socket unless configured as
/// loopback TCP (tests, and the documented Windows shape — SA §4.2).
///
/// The bound addresses are announced on stdout before any plane accepts a connection; see
/// [`serve_announcing`] for the rule that keeps that line readable.
pub async fn run(prepared: Prepared) -> Result<(), BoxError> {
    serve_announcing(prepared, std::io::stdout()).await
}

/// [`run`], with the announce line written somewhere a test can read.
///
/// **Stdout carries the announce line and nothing else.** The line is a single JSON object,
/// `{"event":"listening","viewer":…,"session":…,"control":…}`, written and flushed once every
/// plane is bound and every router is built, so a supervisor that has read it can send a request
/// immediately. A unix-socket control plane reports `unix:` and its path. The process's own
/// diagnostics go to stderr (`tessera serve` mounts the tracing subscriber there), which is what
/// makes the first stdout line a supervisor can rely on.
pub async fn serve_announcing<W: std::io::Write>(
    prepared: Prepared,
    mut announce_to: W,
) -> Result<(), BoxError> {
    let Prepared { state, config } = prepared;

    // `prepare` refused a deployment declaring no addresses, so these are present by construction.
    let viewer_addr = config
        .viewer_addr
        .expect("prepare() refuses a serve with no viewer address");
    let session_addr = config
        .session_addr
        .expect("prepare() refuses a serve with no session address");
    let viewer_listener = tokio::net::TcpListener::bind(viewer_addr).await?;
    let session_listener = tokio::net::TcpListener::bind(session_addr).await?;

    // Bound before the announce, so the line names three live planes rather than two.
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
