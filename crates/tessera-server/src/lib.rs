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
pub mod cors;
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

/// Task 5: refuse to start unless each cache bound admits at least `expected_concurrent_sessions`
/// entries at the measured per-entry size.
///
/// **Why a refusal and not a warning.** The miss/hit cost ratio here is 10⁵–10⁷: a projection miss
/// is `RowProjection::new`, *measured* in seconds (the 10⁹ warm-up viewport is 10.7 s), and every
/// ≥25%-coverage mask at 10⁹ serialises to a *measured* 125.12 MB. A bound below the working set
/// does not degrade the hit rate gently — under a cyclic access pattern LRU's hit rate is exactly
/// zero, every request pays a rebuild, and because misses hold an admission permit for their whole
/// multi-second build the gate saturates and *warm* requests are shed too. So the bound has to be
/// right, and the only place to insist on it is before the listener binds.
///
/// **A policy correction, because the earlier version of this comment was written to be cited and
/// was wrong.** It claimed "there is no policy that fixes that (random replacement gets ≈ C/N and
/// nothing gets more)" — i.e. that no policy which caches each miss beats zero under a pure cycle.
/// That is false. LRU (and FIFO) get exactly zero because the victim is always the very next key to
/// be requested; but **MRU**, and the cold-end/midpoint insertion the Task 5 design considered and
/// declined, both cache each miss and still retain a *fixed* resident set of about `C − 1` of the
/// `N` keys, because the entries at the protected end are never chosen as victims. Simulate cold-
/// end insertion at `C = 3`, `N = 5`: keys 1 and 2 survive every cycle and the hit rate settles at
/// 2/5, against LRU's exactly 0.
///
/// **The refusal still stands, on grounds that do not depend on the wrong claim.** `(C − 1)/N` is
/// not a rescue at this cost ratio: the unlucky `N − C + 1` keys pay the full multi-second rebuild
/// on *every* pan, holding an admission permit while they do it, so the gate saturation this
/// refusal exists to prevent happens anyway — it merely spares some sessions. A configuration whose
/// defence is "most sessions are fine" is one to refuse at startup, not to soften with a policy,
/// and `CacheStats::young_evictions` alarms if the regime is entered another way. The correction is
/// recorded rather than dropped because the design records this as a deliberate non-choice "with
/// its arithmetic, so it is not rediscovered as an oversight", and arithmetic that is wrong is
/// worse than none.
///
/// The relation is asserted in two places for two different reasons, which is deliberate rather
/// than duplication: `config::defaults_satisfy_task_5s_cache_relation` pins it for the *defaults*
/// at build time, so an edit to one constant cannot silently break it; this pins it for the
/// *operator's* file at startup. Both read [`config::MEASURED_PROJECTION_BYTES_AT_1E9`], which is
/// where the figure's provenance is documented.
///
/// **Both caches, not just the projection one** (Task 0 gate, F11): they hold the same-shaped
/// Roaring object at the same measured size, and a validation covering one leaves the other free to
/// be set to a collapsing value. The two bounds' entry counts are governed by different quantities
/// — sessions against distinct grant sets — and their constants say so.
///
/// **This is a floor, not a sizing.** `DEFAULT_ROW_PROJECTION_CACHE_BYTES` carries a further 2× for
/// entry-count headroom (a second slice, or a generation swap's transient duplicate); passing this
/// check at exactly 1× is admissible but leaves none. And neither bound is a memory *budget*: peak
/// is `bound + compute_admission × per_entry`, which at 48-way admission is another ~6 GB — see
/// `tessera_engine`'s `RowProjectionCache` doc, where that arithmetic lives with its operand.
///
/// **The per-entry figure is the 10⁹ one whatever the corpus is**, deliberately, and the message
/// says so: this constant is not read from the bundle, so an operator serving a 10⁶-row corpus
/// whose real projections are a few tens of megabytes in total is still asked for a bound in the
/// gigabytes. That is a *ceiling*, not an allocation — the cache holds what it holds and the bound
/// is only a refusal threshold, so an over-large bound costs nothing until the entries exist. It is
/// left conservative rather than scaled because the figure that matters is the one this deployment
/// could reach after a growth, and because scaling it would mean deriving a per-entry size from a
/// manifest that does not carry one.
fn validate_cache_bounds(config: &Config) -> Result<(), BoxError> {
    let per_entry = config::MEASURED_PROJECTION_BYTES_AT_1E9;
    // `checked_mul`, because this product is `u64 × u64` from an operator's file: at the parse-time
    // ceiling on `expected_concurrent_sessions` it cannot overflow today, but a wrap would produce
    // a *small* working set, i.e. it would silently admit exactly the collapsing configuration this
    // function exists to refuse. A refusal is the only safe answer to an unrepresentable one.
    let Some(working_set) = (config.expected_concurrent_sessions as u64).checked_mul(per_entry)
    else {
        return Err(format!(
            "serve.expected_concurrent_sessions = {} × the measured {per_entry} B per entry \
             overflows a u64, so no cache bound could satisfy it. Refusing to start: lower \
             serve.expected_concurrent_sessions to the concurrency you actually expect.",
            config.expected_concurrent_sessions
        )
        .into());
    };
    for (key, value) in [
        (
            "serve.row_projection_cache_bytes",
            config.row_projection_cache_bytes,
        ),
        ("serve.fragment_cache_bytes", config.fragment_cache_bytes),
    ] {
        if value < working_set {
            return Err(format!(
                "{key} = {value} B admits fewer than serve.expected_concurrent_sessions = {} \
                 entries at the measured {per_entry} B per entry ({working_set} B needed). \
                 Refusing to start: a cache bound below the working set does not lower the hit \
                 rate, it collapses it — every request pays a multi-second rebuild while holding \
                 an admission permit, so the gate saturates and warm requests are shed too. Raise \
                 {key} to at least {working_set}, or lower serve.expected_concurrent_sessions if \
                 that is genuinely the concurrency you expect. Note that {key} is a CEILING, not \
                 an allocation: nothing is reserved, and a bound larger than the corpus can fill \
                 costs nothing. The {per_entry} B figure is measured at 10⁹ rows and is not scaled \
                 to this bundle, so a small corpus is asked for a bound far above its real working \
                 set — deliberately, because the number that matters is the one this deployment \
                 could reach.",
                config.expected_concurrent_sessions
            )
            .into());
        }
    }
    Ok(())
}

/// Load config and open the engine. Fails closed: a missing `[disclosure]` section, an
/// unreadable bundle, or a WAL that fails the positional CRC rule all return `Err` here, before
/// any socket is ever bound.
///
/// (This doc belongs to `prepare`. Task 5 inserted [`validate_cache_bounds`] between the two and
/// left it heading that private function, so the crate's entry point had no doc at all.)
pub fn prepare(config_path: &Path) -> Result<Prepared, BoxError> {
    let config = config::load(config_path)?;
    validate_cache_bounds(&config)?;

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
    // Phase 2 stage 2.1, Task 5: the two cache bounds, validated above. Set here through a method
    // rather than carried in `EngineConfig` for the same reason `ingest_queue_bound` is — three
    // exhaustive `EngineConfig` literals live in `crates/tessera-engine/tests/viewport.rs`, which
    // this stage's allowlist freezes for every track, so a new field would make the workspace
    // uncompilable with no in-allowlist repair. See `Engine::set_cache_bounds`.
    engine.set_cache_bounds(
        config.row_projection_cache_bytes,
        config.fragment_cache_bytes,
    );
    // Phase 2 stage 2.1, Task 6 (D5): `overlay_soft_limit`'s consumer. Additive for the same
    // reason `set_cache_bounds` is — widening `start_write_executor` would touch two crates'
    // call sites outside this stage's allowlist. **It alarms; it does not act**: there is no fold
    // until stage 2.3. Set after replay, and the setter evaluates the predicate once as it lands,
    // so a node that replayed a WAL already over the limit alarms at startup rather than waiting
    // for the next deny.
    engine.set_overlay_soft_limit(config.overlay_soft_limit);
    // Phase 2 stage 2.1, Task 7a: `commit_window_max_items`' consumer — the row count at which a
    // commit window closes, and with it the scope of design §11.1's signature sort. Additive for
    // the same reason as the two above.
    engine.set_commit_window_max_rows(config.commit_window_max_items);
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
        // Task 6 (D2/D1): the control plane's own bounds. Deliberately separate from
        // `compute_gate` — D13 keeps the control plane out of the viewer gate.
        ingest_admission: state::IngestAdmission::new(config.ingest_admission),
        ingest_max_batch_rows: config.ingest_max_batch_rows,
        ingest_max_batch_bytes: config.ingest_max_batch_bytes,
        stage_timing: config.stage_timing,
        min_visible_members: config.min_visible_members,
        session_credential: config.session_credential.clone(),
        operator_credential: config.operator_credential.clone(),
        dev_cors_origins: config.dev_cors_origins.clone(),
    });

    // MVP client spec §3. Loud, and at `warn`, because the key's effect is to let a page from
    // another origin present a session token and the session credential to this process. It is a
    // development affordance; T2 with verified assertions remains the documented integration
    // topology (client-interaction §7).
    if !config.dev_cors_origins.is_empty() {
        tracing::warn!(
            origins = ?config.dev_cors_origins,
            "serve.dev_cors_origins is set: these browser origins may present session tokens and \
             the session credential to this process. This is a DEVELOPMENT affordance — do not \
             enable it in a deployment."
        );
    }

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
