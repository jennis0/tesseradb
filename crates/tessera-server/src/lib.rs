//! `tessera-server` — the three HTTP planes (viewer/session/control), fail-closed config, and the
//! `tessera serve` entry point.
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
mod filter_dto;
pub mod health;
pub mod session;
pub mod state;
pub mod viewer;

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;

use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

use config::{Config, ConfigError, ControlListen};
use state::{AppState, ComputeGate, SessionRegistry};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Everything [`prepare`] built: the shared state and the resolved config it needs to bind
/// listeners in [`run`].
pub struct Prepared {
    pub state: Arc<AppState>,
    pub config: Config,
}

/// Refuse to start unless each cache bound admits at least `expected_concurrent_sessions` entries
/// at the measured per-entry size.
///
/// **Why a refusal and not a warning.** The miss/hit cost ratio here is 10⁵–10⁷: a projection miss
/// is `RowProjection::new`, *measured* at 1 277 ms at 10⁹ (`probes/2026-08-14-project-decomposition/`), and every
/// ≥25%-coverage mask at 10⁹ serialises to a *measured* 125.12 MB. A bound below the working set
/// does not degrade the hit rate gently — under a cyclic access pattern LRU's hit rate is exactly
/// zero, every request pays a rebuild, and because misses hold an admission permit for their whole
/// multi-second build the gate saturates and *warm* requests are shed too. So the bound has to be
/// right, and the only place to insist on it is before the listener binds.
///
/// **A replacement policy is not the alternative to this refusal, and the arithmetic is worth
/// stating so it is not proposed as one.** LRU (and FIFO) get exactly zero on a cycle longer than
/// the cache because the victim is always the very next key to be requested. **MRU**, and the
/// cold-end/midpoint insertion this cache declines, do better: both cache each miss and retain a
/// *fixed* resident set of about `C − 1` of the `N` keys, because entries at the protected end are
/// never chosen as victims — simulate cold-end insertion at `C = 3`, `N = 5` and keys 1 and 2
/// survive every cycle, settling at a hit rate of 2/5 against LRU's exactly 0. But `(C − 1)/N` is
/// not a rescue at this cost ratio: the unlucky `N − C + 1` keys pay the full multi-second rebuild
/// on *every* pan, holding an admission permit while they do it, so the gate saturation this
/// refusal exists to prevent happens anyway — it merely spares some sessions. A configuration whose
/// defence is "most sessions are fine" is one to refuse at startup, not to soften with a policy,
/// and `CacheStats::young_evictions` alarms if the regime is entered another way.
///
/// The relation is asserted in two places for two different reasons, which is deliberate rather
/// than duplication: `config::defaults_satisfy_task_5s_cache_relation` pins it for the *defaults*
/// at build time, so an edit to one constant cannot silently break it; this pins it for the
/// *operator's* file at startup. Both read [`config::MEASURED_PROJECTION_BYTES_AT_1E9`], which is
/// where the figure's provenance is documented.
///
/// **Both caches, not just the projection one**: they hold the same-shaped Roaring object at the
/// same measured size, and a validation covering one leaves the other free to be set to a
/// collapsing value. The two bounds' entry counts are governed by different quantities — sessions
/// against distinct grant sets — and their constants say so.
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

/// **§4's relation 2**: a merge's byte cap must be strictly below the base segment's size.
///
/// A merge bounded at or above the base could consume it, and a merge that consumes the base is
/// compaction under another name — it pays a full permutation rewrite and re-emits every column,
/// banks none of compaction's benefit, and leaves `MANIFEST.files` digesting files nothing
/// references (§5.3). **The base is not excluded by a rule; it is excluded by this bound**, which
/// is why the bound is validated rather than assumed.
///
/// The base segment is the largest in each slice — a flush segment is one tick's arrivals — so the
/// comparison is against the largest segment the deployment holds.
fn validate_merge_size_relation(config: &Config, engine: &Engine) -> Result<(), BoxError> {
    let generation = engine.generation();
    let base_segment_bytes = generation
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.slices.values())
        .flat_map(|s| s.segments.iter())
        .map(|s| s.columns.byte_len() + s.morton.byte_len())
        .max()
        .unwrap_or(0);
    // Only an **explicitly set** value is checked. An unset one is derived from this same figure
    // when merge selection lands, so it cannot violate the relation — and no fixed default could
    // satisfy it across deployment sizes, which is the whole reason there is not one.
    let Some(max_merged_segment_bytes) = config.max_merged_segment_bytes else {
        return Ok(());
    };
    if base_segment_bytes > 0 && max_merged_segment_bytes >= base_segment_bytes {
        return Err(Box::new(ConfigError::MergeSizeRelation {
            max_merged_segment_bytes,
            base_segment_bytes,
        }));
    }
    Ok(())
}

/// Load config and open the engine. Fails closed: a missing `[disclosure]` section, an
/// unreadable bundle, or a WAL that fails the positional CRC rule all return `Err` here, before
/// any socket is ever bound.
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
        // `serve.compute_threads` — validated at parse time, refused at `0`
        // (`ConfigError::ComputeThreadsZero`) — sizes `Engine::open`'s shared rayon pool, the
        // parallel-sweep CPU bound. Distinct from `compute_admission` (below), which bounds
        // in-flight *requests*, not CPU, and defaults to a multiple of this number
        // (`COMPUTE_ADMISSION_MULTIPLIER`): admitted requests may oversubscribe this pool during
        // their serialise phase, deliberately, because small requests are latency-bound on
        // scheduling rather than CPU.
        compute_threads: config.compute_threads,
        // The flush tick: the one write-path cadence, and the bound on how stale an acknowledged
        // item's absence may be (write-path §4.1).
        flush_max_age_secs: config.flush_max_age_secs,
        // Validated against write-path §7's base-segment relation in `validate_merge_size`, and
        // now delivered: before this it was checked and dropped.
        max_merged_segment_bytes: config.max_merged_segment_bytes,
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
    // **§4's relation 2**, checked here rather than in `config::load` because its right-hand side
    // is a property of the deployment's data: the base segment's size is knowable only once the
    // bundle is open. Same standard as relation 1 — refused, never clamped.
    validate_merge_size_relation(&config, &engine)?;
    // Move the WAL onto its own thread and open the two write queues. Started here rather than
    // inside `Engine::open` so that an engine which never ingests — every read-only test, bench,
    // example and embedder — starts no thread at all. This is `ingest_queue_bound`'s only consumer.
    //
    // Nothing is stored in `AppState`: the engine owns the handle, so `/control/*` reaches the
    // executor through `state.engine` exactly as it reached the WAL before.
    engine.start_write_executor(config.ingest_queue_bound)?;
    // The two cache bounds, validated above.
    engine.set_cache_bounds(
        config.row_projection_cache_bytes,
        config.fragment_cache_bytes,
    );
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
        max_k: config.max_k,
        max_category_values: config.max_category_values,
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
        ingest_max_batch_rows: config.ingest_max_batch_rows,
        ingest_buffer_max_items: config.ingest_buffer_max_items,
        ingest_max_batch_bytes: config.ingest_max_batch_bytes,
        stage_timing: config.stage_timing,
        stream_flush_bytes: config.stream_flush_bytes,
        stream_write_stall_ms: config.stream_write_stall_ms,
        stream_deadline_ms: config.stream_deadline_ms,
        min_visible_members: config.min_visible_members,
        session_credential: config.session_credential.clone(),
        operator_credential: config.operator_credential.clone(),
        dev_cors_origins: config.dev_cors_origins.clone(),
    });

    // Loud, and at `warn`, because the key's effect is to let a page from another origin present a
    // session token and the session credential to this process. It is a development affordance;
    // T2 (server-mediated, with verified assertions) remains the documented integration topology —
    // client-interaction §7 tabulates the four topologies and names the two anti-patterns.
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
