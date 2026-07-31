//! Fail-closed configuration: `tessera.toml` (SA §7).
//!
//! `[disclosure]` has no defaults at all: absence of the section, or of either key inside it, is
//! a startup error naming design §7.5/§2.3 — `min_visible_members` is parsed and stored even
//! though nothing consumes it until Phase 3; the startup rule, not the value, is the point.
//! Every other section either has a documented default (`max_k = 1000`) or is required outright.
//! Credentials are never inline: `[serve]`'s `*_credential_file`/`*_credential_env` pairs are the
//! only way to supply the session/operator bearer secrets.
//!
//! ## The Phase 2 stage-2.1 knobs (Task 0b)
//!
//! Fourteen keys land here *before* the code that reads them, so no parallel track has to edit
//! this file mid-stream (stage-2.1 plan, "How the work parallelises", rule 2). Every one of them
//! is a **performance knob, not a disclosure control**, so under SA §7's rule ("performance knobs
//! default; disclosure controls do not") every one of them defaults, and
//! [`tests::every_stage_2_1_knob_defaults`] pins that reading. They still refuse a **zero**, which
//! is a different thing: a zero is degenerate for every one of these (see
//! [`ConfigError::MustBeNonZero`]), and this file's established discipline is to refuse rather
//! than clamp so a typo cannot silently disable a mechanism.
//!
//! Sectioning follows SA §7's own sketch: the write-path knobs sit under `[ingest]` (where SA §7
//! already puts `flush_max_items`, `flush_max_age` and `overlay_soft_limit`), the serving-side
//! ones under `[serve]` beside the other serving knobs. Units are explicit and consistent in the
//! *name* (`_ms`, `_secs`, `_bytes`) rather than in a duration string — a deviation from SA §7's
//! `"60s"` sketch, taken deliberately so a value's unit survives being read out of a log line or
//! a status payload without its key.
//!
//! **Two of the fourteen are inert this stage**: `flush_max_items` and `flush_max_age_secs` are
//! parsed, validated and stored, and *nothing reads them*, because flush does not exist until
//! stage 2.2. [`tests::the_flush_knobs_are_inert`] asserts that mechanically, so an operator
//! cannot set one and believe it works without this file's doc having been changed first.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Configuration failures. Every variant here is fail-closed: the process must not start serving
/// with a guessed or defaulted value in place of a missing one.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    /// The `[disclosure]` section is absent entirely.
    MissingDisclosureSection,
    /// The `[disclosure]` section is present but missing one of its two required keys.
    MissingDisclosureKey(&'static str),
    /// Neither `*_credential_file` nor `*_credential_env` was set for this credential, or the
    /// named file/env var could not be read.
    MissingCredential(&'static str),
    BadAddr(String),
    /// Phase 1 ships only `builtin:passthrough` (the wasmtime host is out of scope).
    UnsupportedPlugin(String),
    /// `serve.k_min = 0`, which switches off §7.2's floor clause — the **I7 guarantee** that a
    /// non-empty tile always draws at least one mark. With no floor, a tile whose visible items all
    /// sit above θ serves nothing, and the sparsest principals' maps go blank exactly where I7
    /// exists to keep them populated. Refused at startup rather than clamped, so a typo cannot
    /// quietly disable an invariant.
    FloorClauseDisabled,
    /// `serve.k_min` exceeds a cap that will clamp it — either `k_max_marks` (the overplot ceiling)
    /// or `max_k` (the machine ceiling). Both clamp the floor, since the effective cap is
    /// `min(k, max_k, k_max_marks)` and the floor is `min(k_min, cap)`. Well-defined either way, but
    /// it means one of the two numbers is not doing what its author thought.
    FloorAboveCap {
        k_min: usize,
        cap_name: &'static str,
        cap: usize,
    },
    /// `serve.max_underlay_offset` above the grid's own depth. The §5.2 grid is 2¹⁶ × 2¹⁶, so an
    /// offset beyond 16 can never be usable at any zoom — every request naming it would be refused
    /// at the depth check. It is refused at startup instead, because the value also feeds a
    /// `1 << (2 * offset)` shift and this file validates every other selection constant; leaving one
    /// shift input unbounded is an inconsistent standard rather than a considered exemption.
    UnderlayOffsetTooDeep(u8),
    /// `serve.theta_target_marks = 0`, which anchors θ at `Cut(0)` — a threshold that admits
    /// **nothing**, at every depth, because `0u64.leading_zeros() == 64` so the per-depth shift
    /// always "fits". Every non-empty tile would then draw exactly `k_min` marks at every zoom
    /// forever, with no error raised anywhere: design §7.2's density signal silently gone.
    ///
    /// This is the *same* silent failure mode `Threshold::at_depth`'s `leading_zeros` check exists to
    /// prevent, reachable through config instead of through a shift bug — so it is refused in the
    /// same spirit.
    ThetaTargetZero,
    /// `serve.compute_threads = 0` (D-B): the pool this knob sizes must fill the machine, and a
    /// zero-width pool can run nothing at all. Refused rather than clamped to 1, so a typo cannot
    /// quietly turn "one thread per core" into "one thread total".
    ComputeThreadsZero,
    /// `serve.compute_admission = 0` (D-B): the compute semaphore would have zero permits, so
    /// every gated request sheds unconditionally — indistinguishable from the server being down,
    /// but silently. Refused rather than clamped to 1 for the same reason as the floor clause.
    ComputeAdmissionZero,
    /// `serve.admission_timeout_ms = 0` (D-E) would silently disable the bounded queue wait —
    /// every request either starts immediately or sheds instantly, with no queueing at all, which
    /// is what `serve.compute_queue = 0` (a legal value) already expresses explicitly. Refused so
    /// a zero here reads as a mistake rather than a second spelling of that same knob.
    AdmissionTimeoutZero,
    /// `serve.compute_admission + serve.compute_queue` overflows `usize`, or the sum exceeds
    /// `tokio::sync::Semaphore::MAX_PERMITS` (`usize::MAX >> 3`) — the two knobs together size
    /// the outer slots semaphore `AppState::new` builds
    /// (`Semaphore::new(compute_admission + compute_queue)`), and `Semaphore::new` panics past
    /// that bound. Refused here, at config parse, rather than left to panic during server
    /// startup for an absurd but syntactically valid `tessera.toml`.
    ComputeAdmissionQueueOverflow {
        compute_admission: usize,
        compute_queue: usize,
    },
    /// `serve.compute_admission` was left to default and `COMPUTE_ADMISSION_MULTIPLIER *
    /// compute_threads` overflows `usize` — an operator-supplied `compute_threads` extreme enough
    /// to overflow here would silently wrap to a small, wrong permit count in a release build
    /// (overflow checks are off), the same class of silent failure
    /// `ComputeAdmissionQueueOverflow`'s checked-add exists to prevent one step downstream.
    /// Refused here, at the multiplication itself, for the same reason.
    ComputeAdmissionDefaultOverflow {
        compute_threads: usize,
    },
    /// One of the Phase 2 stage-2.1 knobs (Task 0b) was set to `0`, and `0` is degenerate for
    /// every one of them — not "off", but "on and silently useless". `consequence` names the
    /// specific silent failure at the check site, because a generic "must be non-zero" tells an
    /// operator what to type and nothing about what they nearly did.
    ///
    /// Same refuse-rather-than-clamp discipline as [`ConfigError::FloorClauseDisabled`] and
    /// [`ConfigError::ComputeThreadsZero`]: clamping means a typo silently changed the
    /// configuration, passing it through means a typo silently disabled a mechanism.
    MustBeNonZero {
        key: &'static str,
        consequence: &'static str,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io error: {e}"),
            ConfigError::Toml(e) => write!(f, "config parse error: {e}"),
            ConfigError::MissingDisclosureSection => write!(
                f,
                "tessera.toml is missing its [disclosure] section — design §7.5/§2.3: \
                 disclosure parameters have no defaults, so startup refuses rather than silently \
                 choosing one"
            ),
            ConfigError::MissingDisclosureKey(key) => write!(
                f,
                "tessera.toml's [disclosure] section is missing '{key}' — design §7.5/§2.3: \
                 disclosure parameters have no defaults, so startup refuses rather than silently \
                 choosing one"
            ),
            ConfigError::MissingCredential(which) => write!(
                f,
                "no credential configured for '{which}' — set *_credential_file or \
                 *_credential_env in [serve], never inline in tessera.toml"
            ),
            ConfigError::BadAddr(raw) => write!(f, "not a valid listen address: '{raw}'"),
            ConfigError::UnsupportedPlugin(module) => write!(
                f,
                "unsupported plugin module '{module}' — Phase 1 ships only builtin:passthrough \
                 (the wasmtime host is out of scope)"
            ),
            ConfigError::FloorClauseDisabled => write!(
                f,
                "serve.k_min = 0 switches off design §7.2's floor clause, which is the I7 \
                 guarantee that a non-empty tile always draws at least one mark; with no floor a \
                 tile whose visible items all sit above the threshold serves nothing. Startup \
                 refuses rather than clamping, so a typo cannot quietly disable an invariant"
            ),
            ConfigError::FloorAboveCap {
                k_min,
                cap_name,
                cap,
            } => write!(
                f,
                "serve.k_min ({k_min}) exceeds serve.{cap_name} ({cap}) — the floor would be \
                 clamped to that cap on every tile, so one of the two is not doing what its author \
                 intended"
            ),
            ConfigError::UnderlayOffsetTooDeep(offset) => write!(
                f,
                "serve.max_underlay_offset ({offset}) exceeds the grid's own depth of 16 (§5.2), so \
                 no zoom could ever use it"
            ),
            ConfigError::ThetaTargetZero => write!(
                f,
                "serve.theta_target_marks = 0 anchors design §7.2's threshold at a cut that admits \
                 nothing, at every depth — so every non-empty tile would draw exactly k_min marks \
                 at every zoom, with the density signal silently gone. Startup refuses rather than \
                 serving a map that looks plausible and conveys nothing"
            ),
            ConfigError::ComputeThreadsZero => write!(
                f,
                "serve.compute_threads = 0 — the compute pool must fill the machine (D-B); a \
                 zero-width pool can run nothing. Startup refuses rather than clamping to 1"
            ),
            ConfigError::ComputeAdmissionZero => write!(
                f,
                "serve.compute_admission = 0 — the compute semaphore would have zero permits, so \
                 every gated request would shed unconditionally (D-B). Startup refuses rather than \
                 clamping to 1"
            ),
            ConfigError::AdmissionTimeoutZero => write!(
                f,
                "serve.admission_timeout_ms = 0 would silently disable the bounded queue wait \
                 (D-E) — use serve.compute_queue = 0 to disable queueing explicitly instead"
            ),
            ConfigError::ComputeAdmissionQueueOverflow {
                compute_admission,
                compute_queue,
            } => write!(
                f,
                "serve.compute_admission ({compute_admission}) + serve.compute_queue \
                 ({compute_queue}) overflows usize or exceeds tokio::sync::Semaphore::MAX_PERMITS \
                 ({}) — the outer slots semaphore cannot be built at this size; lower one or both \
                 knobs",
                tokio::sync::Semaphore::MAX_PERMITS
            ),
            ConfigError::ComputeAdmissionDefaultOverflow { compute_threads } => write!(
                f,
                "serve.compute_threads ({compute_threads}) is too large: the default \
                 serve.compute_admission = {COMPUTE_ADMISSION_MULTIPLIER} * compute_threads \
                 overflows usize — set serve.compute_admission explicitly to a sane value instead"
            ),
            ConfigError::MustBeNonZero { key, consequence } => write!(
                f,
                "{key} = 0 is degenerate, not 'off': {consequence}. Startup refuses rather than \
                 clamping, so a typo cannot quietly disable a mechanism"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Deserialize)]
struct RawConfig {
    bundle: RawBundle,
    plugin: RawPlugin,
    #[serde(default)]
    disclosure: Option<toml::Value>,
    serve: RawServe,
    /// SA §7's `[ingest]` section — every key optional, so the whole section may be absent. These
    /// are the write path's performance knobs; none of them is a disclosure control.
    #[serde(default)]
    ingest: RawIngest,
}

/// SA §7's `[ingest]` section (Task 0b). Every field is `Option` and the struct is `Default`, so
/// a `tessera.toml` with no `[ingest]` section at all parses to "every key defaulted".
#[derive(Deserialize, Default)]
struct RawIngest {
    #[serde(default)]
    commit_window_max_items: Option<usize>,
    #[serde(default)]
    commit_window_max_age_ms: Option<u64>,
    #[serde(default)]
    ingest_queue_bound: Option<usize>,
    #[serde(default)]
    ingest_max_batch_rows: Option<usize>,
    #[serde(default)]
    ingest_max_batch_bytes: Option<usize>,
    #[serde(default)]
    wal_hard_limit_bytes: Option<u64>,
    #[serde(default)]
    overlay_soft_limit: Option<usize>,
    #[serde(default)]
    flush_max_items: Option<usize>,
    #[serde(default)]
    flush_max_age_secs: Option<u64>,
}

#[derive(Deserialize)]
struct RawBundle {
    path: PathBuf,
    cache: PathBuf,
    wal: PathBuf,
}

#[derive(Deserialize)]
struct RawPlugin {
    module: String,
}

#[derive(Deserialize)]
struct RawServe {
    viewer: String,
    session: String,
    control: String,
    #[serde(default)]
    max_k: Option<usize>,
    /// Emit the `x-tessera-stage-ns` breakdown header. Defaults to **false**, and has no effect
    /// at all unless the binary was also built with the `bench-timing` feature.
    #[serde(default)]
    stage_timing: Option<bool>,
    #[serde(default)]
    k_min: Option<usize>,
    #[serde(default)]
    k_max_marks: Option<usize>,
    #[serde(default)]
    theta_target_marks: Option<u64>,
    #[serde(default)]
    max_underlay_offset: Option<u8>,
    #[serde(default)]
    max_underlay_cells: Option<usize>,
    #[serde(default)]
    max_tiles_per_request: Option<usize>,
    #[serde(default)]
    session_credential_file: Option<PathBuf>,
    #[serde(default)]
    session_credential_env: Option<String>,
    #[serde(default)]
    operator_credential_file: Option<PathBuf>,
    #[serde(default)]
    operator_credential_env: Option<String>,
    #[serde(default)]
    compute_threads: Option<usize>,
    #[serde(default)]
    compute_admission: Option<usize>,
    #[serde(default)]
    compute_queue: Option<usize>,
    #[serde(default)]
    admission_timeout_ms: Option<u64>,
    #[serde(default)]
    row_projection_cache_bytes: Option<u64>,
    #[serde(default)]
    fragment_cache_bytes: Option<u64>,
    #[serde(default)]
    expected_concurrent_sessions: Option<usize>,
    #[serde(default)]
    pin_ttl_secs: Option<u64>,
    #[serde(default)]
    pins_per_session_max: Option<usize>,
}

/// The control plane's listen target: a real unix socket, or (tests, and the documented Windows
/// shape — SA §4.2) a loopback TCP address, since `reqwest` does not speak unix sockets.
#[derive(Debug, Clone)]
pub enum ControlListen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bundle_path: PathBuf,
    pub cache_dir: PathBuf,
    pub wal_path: PathBuf,
    /// Parsed and stored, consumed by nothing until Phase 3 — the startup rule is the point
    /// (design §7.5/§2.3).
    pub min_visible_members: u64,
    pub token_max_lifetime_secs: u64,
    pub viewer_addr: SocketAddr,
    pub session_addr: SocketAddr,
    pub control_listen: ControlListen,
    pub max_k: usize,
    /// §7.2's floor clause. See `EngineConfig::k_min`.
    pub k_min: usize,
    /// §7.2's cap clause — the *overplot* ceiling, distinct from `max_k`'s machine ceiling. See
    /// `EngineConfig::k_max_marks`.
    pub k_max_marks: usize,
    /// θ's anchor target. See `EngineConfig::theta_target_marks`.
    pub theta_target_marks: u64,
    pub max_underlay_offset: u8,
    pub max_underlay_cells: usize,
    /// Availability bound on the base viewport path. See `EngineConfig::max_tiles_per_request`.
    pub max_tiles_per_request: usize,
    /// Emit `x-tessera-stage-ns` on viewport responses. **Fails closed**: absent means false, and
    /// even true does nothing in a binary built without the `bench-timing` feature. The header
    /// carries only durations and row counts — no identifier, no per-principal label (SA §9) —
    /// but it quantifies the C4 timing channel, so it stays off unless a measurement asked for it.
    pub stage_timing: bool,
    pub session_credential: String,
    pub operator_credential: String,
    /// D-B: the pool this sizes should fill the machine. This task adds the knob and its
    /// validation only — the rayon pool that consumes it arrives in a later task.
    pub compute_threads: usize,
    /// D-B: the compute semaphore's permit count — a bound on in-flight *requests*, admitted for
    /// the viewer/session planes only (never the control plane, D13), not a bound on runnable CPU:
    /// `compute_threads` (the rayon pool) still bounds the parallel-sweep CPU each admitted
    /// request may fan out across, and this gate deliberately lets the serialise phase
    /// oversubscribe up to `compute_admission` because small requests are latency-bound on
    /// scheduling, not CPU. Defaults to [`COMPUTE_ADMISSION_MULTIPLIER`]`× compute_threads` — see
    /// that constant's doc for the measurement behind the multiplier.
    pub compute_admission: usize,
    /// D-B: the outer slots semaphore's *additional* permits beyond `compute_admission` — the
    /// bounded queue. Legally `0` (shed the instant every compute permit is busy). Defaults to
    /// `2 × compute_admission`, now effectively `2 × COMPUTE_ADMISSION_MULTIPLIER = 8×` cores.
    pub compute_queue: usize,
    /// D-E: how long a request may wait for a compute permit before it is shed with 429
    /// `backpressure` and `Retry-After: 1`.
    pub admission_timeout_ms: u64,

    // ---- Phase 2 stage 2.1 (Task 0b). Landed here ahead of their consumers so no track has to
    // edit this file mid-stream. Each field names the task that reads it; the argument for each
    // default is on its `DEFAULT_*` constant.
    /// **Task 7a** — the entry count at which a commit window closes.
    /// See [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`]. `1` is the honest way to disable group commit.
    pub commit_window_max_items: usize,
    /// **Task 7b** — the age at which a commit window closes. See
    /// [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`]; the worst-case deny starvation is **twice** this
    /// (7b rule 3: a deny racing the close decision lands in the next window).
    pub commit_window_max_age_ms: u64,
    /// **Task 6** — the bounded work queue's depth; full is a 429 with `retry_after_s`.
    /// See [`DEFAULT_INGEST_QUEUE_BOUND`]. The deny queue is unbounded and this never bounds it.
    pub ingest_queue_bound: usize,
    /// **Task 6** — per-request row cap on `/control/ingest`; over is 422 (contracts §3.1).
    /// See [`DEFAULT_INGEST_MAX_BATCH_ROWS`].
    pub ingest_max_batch_rows: usize,
    /// **Task 6** — per-request body-byte cap on `/control/ingest`; over is 422. This is the
    /// operand of the headroom assertion (see [`DEFAULT_WAL_HARD_LIMIT_BYTES`]), because a queue
    /// bounded in *entries* bounds nothing without it. See [`DEFAULT_INGEST_MAX_BATCH_BYTES`].
    pub ingest_max_batch_bytes: usize,
    /// **Task 6** — the WAL's byte ceiling, and the right-hand side of the startup headroom
    /// assertion. See [`DEFAULT_WAL_HARD_LIMIT_BYTES`]. No WAL bound existed before this key.
    pub wal_hard_limit_bytes: u64,
    /// **Task 6** — overlay depth at which an alarm is raised. See
    /// [`DEFAULT_OVERLAY_SOFT_LIMIT`]. **Alarms only**: there is no fold until stage 2.3, so
    /// crossing it gets an operator a signal, never relief.
    pub overlay_soft_limit: usize,
    /// **Stage 2.2 — INERT.** Parsed, validated, stored, read by nothing: its consumer is flush,
    /// which does not exist yet. See [`DEFAULT_FLUSH_MAX_ITEMS`].
    pub flush_max_items: usize,
    /// **Stage 2.2 — INERT.** Parsed, validated, stored, read by nothing: its consumer is flush,
    /// which does not exist yet. See [`DEFAULT_FLUSH_MAX_AGE_SECS`].
    pub flush_max_age_secs: u64,
    /// **Task 5** — byte bound on the row-projection cache. See
    /// [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], and [`MEASURED_PROJECTION_BYTES_AT_1E9`] for the
    /// per-entry size the startup validation weighs it against.
    pub row_projection_cache_bytes: u64,
    /// **Task 5** — byte bound on the *in-memory* fragment tier. See
    /// [`DEFAULT_FRAGMENT_CACHE_BYTES`]. The `.frag` sidecar tier is untouched by it.
    pub fragment_cache_bytes: u64,
    /// **Task 5** — the concurrency the projection cache must not collapse at; the startup
    /// validation is that [`Config::row_projection_cache_bytes`] admits at least this many
    /// entries. See [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`].
    pub expected_concurrent_sessions: usize,
    /// **Task 4** — pin TTL (lifecycle §2.2). See [`DEFAULT_PIN_TTL_SECS`].
    pub pin_ttl_secs: u64,
    /// **Task 4** — per-session pin cap (lifecycle §2.2). See
    /// [`DEFAULT_PINS_PER_SESSION_MAX`].
    pub pins_per_session_max: usize,
}

/// The machine ceiling on a viewport's `k` — GPU, transport, handle table.
///
/// **A working value, not a calibration** *(owner, 2026-07-30; was 200)*. The drawn-mark budget
/// spec's probes P1–P3 are what calibrate this number, and they have not run; the identity plan
/// blocks committing a *calibrated* value until its own rebuild lands, because a `k` measured
/// against the pre-rebuild bundle would be measured against the wrong box and the wrong identity
/// width. Nothing about that blocks setting a sane working default in the meantime, and 200 was
/// itself never calibrated either.
///
/// Deliberately **above** [`DEFAULT_K_MAX_MARKS`]: the machine ceiling should not be the binding one
/// — the overplot ceiling should be — so that raising what a screen can legibly show does not also
/// require re-reasoning about transport.
const DEFAULT_MAX_K: usize = 1_000;

/// §7.2's floor clause: the fewest marks a non-empty tile draws. Provisional (density memo §4),
/// pending that memo's §0 visual experiments.
const DEFAULT_K_MIN: usize = 2;

/// §7.2's cap clause: the most marks any one tile draws. **The overplot ceiling** — sized by what a
/// screen can legibly show, not by machine limits.
///
/// *(owner, 2026-07-30; was 128)*. Density memo §4 argued 128 from ink coverage at ~80x80 px per
/// tile, on the premise that a viewport draws a few hundred tiles and that mark count stops reading
/// as density somewhere around 50–100 marks per tile. That premise is **untested** — the memo says
/// so itself, and it is exactly what the drawn-mark budget's P1 probe exists to settle — and it sits
/// against an owner decision that the drawn-mark budget should be the largest a client can render.
/// This value takes the owner's side of that pending the probe.
///
/// It is the number that actually binds: with `k` defaulting to the same value, the effective cap is
/// this. Raising it widens §7.2's proportional window, which is `cap / k_min`, from 64 at the old
/// pair to 250 here.
const DEFAULT_K_MAX_MARKS: usize = 500;

/// The MACHINE ceiling must sit at or above the OVERPLOT ceiling, so the overplot one is what binds.
/// Inverted, raising what a screen can legibly show would silently do nothing until someone also
/// raised a transport limit — a confusing failure, and exactly the conflation these two constants
/// were split apart to prevent. Checked at compile time rather than in a test: it is a property of
/// the two literals, so it should fail the build.
const _: () = assert!(DEFAULT_MAX_K >= DEFAULT_K_MAX_MARKS);

/// θ's anchor target: marks the mean occupied tile should draw at any depth. Provisional.
const DEFAULT_THETA_TARGET_MARKS: u64 = 16;

/// The largest `underlay_offset` a request may ask for (§3.3): sub-cell depth is `zoom + offset`.
const DEFAULT_MAX_UNDERLAY_OFFSET: u8 = 4;

/// The ceiling on sub-cells in one response. `tiles_for_bbox` is itself uncapped and the underlay
/// multiplies its output by `4^offset`, so without this one request can demand ~77k
/// `count_range` calls and blow the 10 ms p99 latency gate.
const DEFAULT_MAX_UNDERLAY_CELLS: usize = 8192;

/// The most tiles one viewport request may span.
///
/// **Sized against the threat, which is unbounded allocation — not against a latency SLO.** Without
/// a bound, zoom 16 over the full extent is 65536² = 4.29e9 tiles at 16 B each, ~69 GB in one `Vec`:
/// an out-of-memory abort from a single authenticated request. At this limit the tile vector is at
/// most 4 MB and the per-tile range arithmetic is bounded with it.
///
/// It is deliberately *not* tightened to the few hundred tiles a real viewport draws. A wide bbox at
/// a deep zoom is an unusual but legitimate query — the differential suite issues them — and its
/// latency is the caller's own and now bounded. Refusing it would trade an availability fix for a
/// functionality regression.
const DEFAULT_MAX_TILES_PER_REQUEST: usize = 262_144;

/// D-B: the compute pool should fill the machine. `available_parallelism` fails only when the OS
/// genuinely cannot answer the question (SA has no fallback story for that host); treated as 1
/// rather than propagated, since a single-threaded fallback still starts the server, and the
/// `ComputeThreadsZero` refusal exists for the case an operator's *explicit* `0` needs catching,
/// not this one.
fn default_compute_threads() -> usize {
    std::thread::available_parallelism().map_or_else(
        |e| {
            // Rare (the OS genuinely could not answer, e.g. an exotic sandboxing setup) but
            // silently sizing the compute pool/admission gate at 1 thread instead of the
            // machine's real core count is a startup-time surprise worth a log line, not a
            // silently-degraded deployment — an operator staring at low throughput later has no
            // other signal that this fallback, rather than an explicit `compute_threads = 1`,
            // is why.
            tracing::warn!(
                error = %e,
                "available_parallelism() failed; falling back to compute_threads = 1 — set \
                 serve.compute_threads explicitly to size the compute pool/admission gate for \
                 this host"
            );
            1
        },
        std::num::NonZeroUsize::get,
    )
}

/// D-E: 25× the 10 ms p99 target — a request that cannot even *start* in 250 ms is better shed
/// with `Retry-After: 1` than served at the measured 1.04 s worst case.
const DEFAULT_ADMISSION_TIMEOUT_MS: u64 = 250;

/// `compute_admission`'s default multiplier over `compute_threads` (resolved, D-B). **Retuned
/// 2026-07-31** (was 1×, "one CPU-bound request per core") on the calibrated-viewport measurement
/// that requests at this corpus scale are ~0.3 ms and mostly memory-bound: `compute_admission`
/// bounds in-flight *requests*, not runnable CPU — the rayon pool (`compute_threads`) still bounds
/// the parallel-sweep CPU each admitted request may fan out across, and the serialise phase
/// deliberately oversubscribes up to `compute_admission`, because small requests are latency-bound
/// on scheduling, not CPU, and a 1× gate left cores idle waiting on the next request rather than
/// running the one already queued.
///
/// Measured (`.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/admission-4x-report.md`, 12-core
/// WSL2 box, 2.42M-row fixture, Arm B): at 1× (`compute_admission=12`) closed-loop throughput was
/// c=5 11,771 / c=100 31,248 / c=1000 28,929 rps against a pre-gate baseline of 15,277 / 48,588 /
/// 49,475 rps, with shed% at c=100/c=1000 around 40%. At 4×, c=100 improves to 35,448 rps
/// (shed% collapses to ~0.1%) and c=1000 to 32,645 rps (shed% to ~1.9%) — a real but partial
/// recovery, not a full one; c=5 is essentially unchanged (11,515 rps) because that cell is
/// latency-/generator-bound, not gate-bound, so a wider gate has nothing to admit that wasn't
/// already getting in. p99 grew 1.02–1.47× over the 1× run across every cell measured, well inside
/// the ~2× bound treated as the retune's own regression limit — the trade this constant makes is
/// real (some tail risk under sustained oversubscription, since the queue is `2 × compute_admission`
/// = 8× cores) but it stayed bounded at this measurement.
const COMPUTE_ADMISSION_MULTIPLIER: usize = 4;

// ---------------------------------------------------------------------------------------------
// Phase 2 stage 2.1 (Task 0b) — the fourteen knobs, and the argument for each default.
//
// The set is chosen as a *consistent* set, not fourteen independent numbers: two of the
// assertions later tasks add are relations between them (Task 6's WAL headroom, Task 5's cache
// admission), and a default configuration that could not start is not a default.
// `defaults_satisfy_task_6s_headroom_relation` and `defaults_satisfy_task_5s_cache_relation`
// pin both relations now, so a later edit to one constant cannot silently make the shipped
// default config refuse to start once those assertions land.
// ---------------------------------------------------------------------------------------------

/// Task 7a: the entry count at which a commit window closes.
///
/// **Sized from the compression arithmetic, which is the window's whole purpose.** The probes'
/// 8.9–36.7× posting compression was measured under a *full-corpus* signature sort; a window
/// realises run lengths ≈ `commit_window_max_items × term_density`, so 10 k entries at the 2%
/// density the corpus shows gives runs of ~200 against a ~1.0 scattered baseline (plan Task 7a,
/// review M1). That is a large win and openly a fraction of the ceiling, and it scales linearly
/// with this number — raising it buys compression and costs window latency and the heap the
/// held rows occupy.
///
/// **`1` is how you disable group commit**, and Task 10's A/B (one large window versus a hundred
/// small ones) needs that spelling to exist. `0` is refused: a window that closes at zero entries
/// is not "off", it is group commit silently doing nothing while the code that implements it
/// still runs.
const DEFAULT_COMMIT_WINDOW_MAX_ITEMS: usize = 10_000;

/// Task 7b: the age at which a commit window closes.
///
/// **The binding constraint is the deny-ack latency, not the ingest latency.** Denies share the
/// window (Task 9), and a deny racing the close decision lands in the *next* one, so the honest
/// worst case is `2 ×` this value — 400 ms here. That sits an order of magnitude inside the
/// owner's write-latency budget (seconds, for ingest *and* denies), which leaves room for the
/// other term in the same sum: the `IngestBuffer` clone, estimated at 100–300 ms per swap at 1 M
/// buffered items (plan Task 7b) and growing linearly with buffer depth while flush is inert.
///
/// The third close trigger — both queues empty — is what keeps this from being a latency *floor*
/// on an idle server, so this number only binds under sustained load, where the item bound above
/// is usually reached first anyway.
const DEFAULT_COMMIT_WINDOW_MAX_AGE_MS: u64 = 200;

/// Task 6: the bounded work queue's depth. Full is a 429 with `retry_after_s`; the deny queue is
/// separate and unbounded, and this never bounds it (lifecycle §1.3's deny priority lane).
///
/// **Bounded by heap, not by taste.** A queued `Command` holds its rows in memory, so the queue's
/// worst case is `ingest_queue_bound × ingest_max_batch_bytes` = 64 × 16 MiB = **1 GiB** — the
/// same ~1 GB envelope the plan's own arithmetic quotes for this knob (bound 100 × 10 k rows ×
/// 1 KB). Memory is already the binding constraint at 10⁹ (the Phase 1 build was OOM-killed at
/// 46.4 GB RSS), so this is deliberately a small number: backpressure that arrives early is a
/// working queue, backpressure that arrives at the OOM killer is not.
const DEFAULT_INGEST_QUEUE_BOUND: usize = 64;

/// Task 6: per-request row cap on `/control/ingest`; over is 422 (contracts §3.1).
///
/// Matched to [`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`] deliberately — one maximal batch is one maximal
/// window's worth of entries, so a single client cannot define the window's size by picking a
/// chunk size, which is exactly the property design §11.1 r23 wants when it moves the sort scope
/// to the server. The `rows` cap is the one that binds for ordinary point data; the byte cap
/// below catches unusually wide rows.
const DEFAULT_INGEST_MAX_BATCH_ROWS: usize = 10_000;

/// Task 6: per-request body-byte cap on `/control/ingest`; over is 422.
///
/// 16 MiB is ~1.6 KB per row at the row cap above — comfortable headroom over the ~1 KB/row the
/// plan's queue arithmetic assumes, so the row cap is what a normal caller meets and this one
/// only catches pathological rows. **Without a byte cap the queue bound bounds nothing** (plan
/// review I-2/I3: a queue bounded in entries let one ten-million-row batch walk straight past
/// it), which is why this key exists at all rather than the row cap alone.
const DEFAULT_INGEST_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Task 6: the WAL's byte ceiling, and the right-hand side of the startup headroom assertion
/// (`ingest_queue_bound × ingest_max_batch_bytes` + reserved deny headroom must sit strictly
/// below it). **No WAL bound existed before this key** — the WAL grew until the filesystem said
/// no, at which point every append fails and, per `WalError::Poisoned`, the handle is dead.
///
/// 8 GiB leaves 7 GiB of headroom above the queue's 1 GiB worst case, so denies — which are never
/// refused for load and therefore have no admission control of their own to fall back on — have
/// somewhere to go even with the ingest queue completely full. It is also small enough to fit the
/// NVMe cache directory SA §7 describes without an operator thinking about it.
const DEFAULT_WAL_HARD_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Task 6: overlay depth at which an alarm is raised. **SA §7's own default**, carried across
/// unchanged.
///
/// **It alarms; it does not act.** There is no fold until stage 2.3, so an operator who sets this
/// today gets a signal that the overlay is deep, not a mechanism that makes it shallower. The
/// depth matters beyond memory: every deny acceptance clones the overlay inside the WAL critical
/// section, so overlay depth is a term in the deny-ack latency that
/// [`DEFAULT_COMMIT_WINDOW_MAX_AGE_MS`] budgets against.
const DEFAULT_OVERLAY_SOFT_LIMIT: usize = 500_000;

/// **INERT this stage.** Stage 2.2's flush trigger by buffered-item count. **SA §7's own
/// default**, carried across unchanged.
///
/// *Its consumer is flush, and flush does not exist yet* — nothing in the process reads this
/// value, and an operator who sets it changes nothing at all. It is landed now only so that
/// stage 2.2 finds the key already parsed, validated and documented rather than adding it to a
/// file four tracks are editing. [`tests::the_flush_knobs_are_inert`] fails the moment anything
/// outside this module names it, which is the moment this paragraph must be deleted.
const DEFAULT_FLUSH_MAX_ITEMS: usize = 100_000;

/// **INERT this stage.** Stage 2.2's flush trigger by age — SA §7's `flush_max_age = "60s"`,
/// carried across with the unit moved into the key name.
///
/// *Its consumer is flush, and flush does not exist yet.* See [`DEFAULT_FLUSH_MAX_ITEMS`]. Worth
/// knowing when 2.2 does wire it: lifecycle §1.2 r4 makes this the bound on **how stale an
/// acknowledged item's absence may be** — a buffered item contributes to no viewport, count or
/// density until flush gives it a row — so it becomes a visibility-latency control, not merely a
/// segment-count one. That is an argument for revisiting the number then, not for pretending it
/// does something now.
const DEFAULT_FLUSH_MAX_AGE_SECS: u64 = 60;

/// The **measured** serialised size of one row projection at the 10⁹ operating point: every
/// mask at ≥25% coverage serialises to a 125.12 MB dense bound (design Appendix A quotes the
/// same 125 MB unsharded figure). Exposed rather than private because it is the operand of Task
/// 5's startup validation, not a documentation flourish.
///
/// Two caveats that belong wherever this number is used: it is the *serialised* size, and
/// `get_serialized_size_in_bytes` underestimates the in-memory footprint of array containers
/// with capacity slack by up to ~2×; and it is the 10⁹ figure, so a smaller corpus leaves the
/// bounds below over-provisioned rather than wrong.
pub const MEASURED_PROJECTION_BYTES_AT_1E9: u64 = 125_120_000;

/// Task 5: byte bound on the row-projection cache.
///
/// **Sized so the cache cannot collapse at the expected concurrency, because plain LRU does not
/// degrade in this regime — it collapses** (plan Task 5, review perf C1). A projection miss is
/// `RowProjection::new`, measured in *seconds* (the 10⁹ warm-up viewport is 10.7 s), so the
/// miss/hit cost ratio is 10⁵–10⁷; and because the single-flight miss path returns
/// `ProjectionBuilding` to every racer, a working set that does not fit presents as a permanent
/// 429 storm with a core set pegged on rebuilds, not as a gently lower hit rate.
///
/// 2 GiB is `2 ×` [`DEFAULT_EXPECTED_CONCURRENT_SESSIONS`] × [`MEASURED_PROJECTION_BYTES_AT_1E9`]
/// (8 × 125 MB ≈ 1 GiB working set). The factor of two is not slack for its own sake: it is
/// exactly the ~2× underestimate the serialised-size accounting carries for array containers
/// with capacity slack.
const DEFAULT_ROW_PROJECTION_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Task 5: byte bound on the **in-memory** fragment tier. The digest-verified `.frag` sidecar
/// tier is untouched by it.
///
/// Deliberately half [`DEFAULT_ROW_PROJECTION_CACHE_BYTES`], and the asymmetry is the point: a
/// fragment miss costs a sidecar re-open plus SHA-256 over ~125 MB (~60–80 ms estimated), where
/// a projection miss costs a rebuild measured in seconds. Fragments are also keyed by canonical
/// grant set rather than by session, so principals with equal grants share one entry and the
/// working set grows with *policy* cardinality, not with concurrency.
const DEFAULT_FRAGMENT_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// Task 5: the session concurrency the projection cache must not collapse at — the startup
/// validation is that `row_projection_cache_bytes` admits at least this many entries at
/// [`MEASURED_PROJECTION_BYTES_AT_1E9`].
///
/// Eight is chosen as the smallest number that is honestly a *deployment* rather than a demo,
/// and it is a floor on the cache bound rather than a limit on sessions: exceeding it is not
/// refused anywhere, it simply means the cache starts evicting. An operator who runs more
/// concurrent sessions than this should raise **both** this and the cache bound together, which
/// is why the validation reads as a relation between them rather than as two independent
/// numbers.
const DEFAULT_EXPECTED_CONCURRENT_SESSIONS: usize = 8;

/// Task 4: how long a session pin stays resolvable (lifecycle §2.2).
///
/// **A pin is expensive in a way its holder cannot see.** A drained generation is held alive by
/// its pin, and at the measured 47.02 GB bundle with a 22.5 GB viewport-hot set, one slow client
/// holding a pin across two compactions contests essentially all of a 47 GB box's page cache and
/// holds up to ~94 GB of *invisible* disk through deleted-but-mapped files (`df` ≠ `du`). The
/// failure is a warm-to-cold cliff — a measured 4.0–4.5 ns/visible-row warm against a modelled
/// 50–100 µs/page-miss — so it arrives as a step, not a slope.
///
/// Five minutes is sized to a human interaction with a frozen view (pan and zoom over one
/// geometry), not to a session: expiry is a `410` and re-pinning is one round trip, so the cost
/// of this being too short is a retry, while the cost of it being too long is the cliff above.
const DEFAULT_PIN_TTL_SECS: u64 = 300;

/// Task 4: the most pins one session may hold at once (lifecycle §2.2).
///
/// The drain-list cost above is *per pin*, so this multiplies it: four is one pin per
/// concurrently-open frozen view with room to spare, and it keeps a single session's worst-case
/// retention to four superseded generations rather than an unbounded number. Exceeding it is
/// refused at the pin, never silently ignored — a session that believes it holds a pin it does
/// not hold would compose against geometry it did not ask for.
const DEFAULT_PINS_PER_SESSION_MAX: usize = 4;

/// Refuses a zero for one of the Task 0b knobs, naming the silent failure that zero would cause.
/// See [`ConfigError::MustBeNonZero`] for why these refuse rather than clamp.
fn non_zero_usize(key: &'static str, value: usize, consequence: &'static str) -> Result<usize> {
    if value == 0 {
        return Err(ConfigError::MustBeNonZero { key, consequence });
    }
    Ok(value)
}

/// [`non_zero_usize`] for the `u64`-typed knobs (byte bounds and durations).
fn non_zero_u64(key: &'static str, value: u64, consequence: &'static str) -> Result<u64> {
    if value == 0 {
        return Err(ConfigError::MustBeNonZero { key, consequence });
    }
    Ok(value)
}

pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    parse(&text)
}

fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;

    if raw.plugin.module != "builtin:passthrough" {
        return Err(ConfigError::UnsupportedPlugin(raw.plugin.module));
    }

    let disclosure_value = raw
        .disclosure
        .ok_or(ConfigError::MissingDisclosureSection)?;
    let table = disclosure_value
        .as_table()
        .ok_or(ConfigError::MissingDisclosureSection)?;
    let min_visible_members = table
        .get("min_visible_members")
        .and_then(toml::Value::as_integer)
        .ok_or(ConfigError::MissingDisclosureKey("min_visible_members"))?
        as u64;
    let token_max_lifetime_secs = table
        .get("token_max_lifetime")
        .and_then(toml::Value::as_integer)
        .ok_or(ConfigError::MissingDisclosureKey("token_max_lifetime"))?
        as u64;

    let viewer_addr: SocketAddr = raw
        .serve
        .viewer
        .parse()
        .map_err(|_| ConfigError::BadAddr(raw.serve.viewer.clone()))?;
    let session_addr: SocketAddr = raw
        .serve
        .session
        .parse()
        .map_err(|_| ConfigError::BadAddr(raw.serve.session.clone()))?;
    let control_listen = parse_control_listen(&raw.serve.control)?;

    let session_credential = load_credential(
        "session",
        raw.serve.session_credential_file.as_deref(),
        raw.serve.session_credential_env.as_deref(),
    )?;
    let operator_credential = load_credential(
        "operator",
        raw.serve.operator_credential_file.as_deref(),
        raw.serve.operator_credential_env.as_deref(),
    )?;

    // §7.2's clause parameters. Every check here refuses rather than clamps: a typo must not
    // silently disable an invariant (the floor) or silently blank the density signal (theta).
    let k_min = raw.serve.k_min.unwrap_or(DEFAULT_K_MIN);
    let k_max_marks = raw.serve.k_max_marks.unwrap_or(DEFAULT_K_MAX_MARKS);
    let max_k = raw.serve.max_k.unwrap_or(DEFAULT_MAX_K);
    let theta_target_marks = raw
        .serve
        .theta_target_marks
        .unwrap_or(DEFAULT_THETA_TARGET_MARKS);
    if k_min == 0 {
        return Err(ConfigError::FloorClauseDisabled);
    }
    // BOTH caps clamp the floor, because the effective cap is `min(k, max_k, k_max_marks)` and the
    // floor is `min(k_min, cap)`. Checking only the overplot ceiling was a gap.
    if k_min > k_max_marks {
        return Err(ConfigError::FloorAboveCap {
            k_min,
            cap_name: "k_max_marks",
            cap: k_max_marks,
        });
    }
    if k_min > max_k {
        return Err(ConfigError::FloorAboveCap {
            k_min,
            cap_name: "max_k",
            cap: max_k,
        });
    }
    if theta_target_marks == 0 {
        return Err(ConfigError::ThetaTargetZero);
    }
    let max_underlay_offset = raw
        .serve
        .max_underlay_offset
        .unwrap_or(DEFAULT_MAX_UNDERLAY_OFFSET);
    if max_underlay_offset > 16 {
        return Err(ConfigError::UnderlayOffsetTooDeep(max_underlay_offset));
    }

    // D-B/D-E's admission knobs. Same refuse-not-clamp discipline as the selection clause above:
    // each of `compute_threads`/`compute_admission`/`admission_timeout_ms` at 0 has a distinct
    // silent-failure mode (an empty pool, a gate that sheds everything, a queue wait that never
    // actually waits) and a typo must not quietly produce any of them. `compute_queue = 0` is
    // legal (D-B) — it means "shed the instant every compute permit is busy" — so it alone is
    // never checked.
    let compute_threads = raw
        .serve
        .compute_threads
        .unwrap_or_else(default_compute_threads);
    if compute_threads == 0 {
        return Err(ConfigError::ComputeThreadsZero);
    }
    let compute_admission = match raw.serve.compute_admission {
        Some(v) => v,
        // `checked_mul`, not `*`: release builds have overflow checks off, so an unchecked
        // multiply here would silently wrap to a small, wrong permit count for an
        // operator-supplied `compute_threads` extreme enough to overflow — refused instead, the
        // same discipline the `compute_admission + compute_queue` checked-add below already
        // applies one step downstream.
        None => compute_threads
            .checked_mul(COMPUTE_ADMISSION_MULTIPLIER)
            .ok_or(ConfigError::ComputeAdmissionDefaultOverflow { compute_threads })?,
    };
    if compute_admission == 0 {
        return Err(ConfigError::ComputeAdmissionZero);
    }
    let compute_queue = raw.serve.compute_queue.unwrap_or(2 * compute_admission);
    // The outer slots semaphore is sized `compute_admission + compute_queue` (`AppState::new`);
    // `Semaphore::new` panics past `MAX_PERMITS`, and a naive `+` panics on overflow first at
    // absurd (but syntactically valid) configured values. Refuse here instead, at parse, so the
    // failure is a typed config error rather than a startup panic.
    match compute_admission.checked_add(compute_queue) {
        Some(total) if total <= tokio::sync::Semaphore::MAX_PERMITS => {}
        _ => {
            return Err(ConfigError::ComputeAdmissionQueueOverflow {
                compute_admission,
                compute_queue,
            })
        }
    }
    let admission_timeout_ms = raw
        .serve
        .admission_timeout_ms
        .unwrap_or(DEFAULT_ADMISSION_TIMEOUT_MS);
    if admission_timeout_ms == 0 {
        return Err(ConfigError::AdmissionTimeoutZero);
    }

    // The Phase 2 stage-2.1 knobs (Task 0b). All fourteen default (SA §7: performance knobs
    // default, disclosure controls do not — none of these is a disclosure control); all fourteen
    // refuse a zero, each with its own silent failure named. No consumer is wired here: every one
    // is read by a later task, and two of them (`flush_*`) by a later *stage*.
    let commit_window_max_items = non_zero_usize(
        "ingest.commit_window_max_items",
        raw.ingest
            .commit_window_max_items
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_ITEMS),
        "a window that closes at zero entries is not group commit switched off, it is group \
         commit silently doing nothing — set it to 1 to disable batching honestly",
    )?;
    let commit_window_max_age_ms = non_zero_u64(
        "ingest.commit_window_max_age_ms",
        raw.ingest
            .commit_window_max_age_ms
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_AGE_MS),
        "a zero-age window closes before anything can join it, so every submission commits alone \
         and the signature sort scope collapses back to whatever chunk size the client picked \
         (design §11.1) — set ingest.commit_window_max_items = 1 to disable batching honestly",
    )?;
    let ingest_queue_bound = non_zero_usize(
        "ingest.ingest_queue_bound",
        raw.ingest
            .ingest_queue_bound
            .unwrap_or(DEFAULT_INGEST_QUEUE_BOUND),
        "a zero-depth work queue is a rendezvous, not a bound: submissions block until the \
         executor picks them up instead of being shed with 429, so backpressure becomes an \
         unbounded wait with no signal",
    )?;
    let ingest_max_batch_rows = non_zero_usize(
        "ingest.ingest_max_batch_rows",
        raw.ingest
            .ingest_max_batch_rows
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_ROWS),
        "every ingest batch would be refused with 422, closing the ingest plane while every \
         health surface still reports the server up",
    )?;
    let ingest_max_batch_bytes = non_zero_usize(
        "ingest.ingest_max_batch_bytes",
        raw.ingest
            .ingest_max_batch_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_BYTES),
        "every ingest batch would be refused with 422, and the queue's headroom arithmetic would \
         conclude that an arbitrarily deep queue fits in any WAL",
    )?;
    let wal_hard_limit_bytes = non_zero_u64(
        "ingest.wal_hard_limit_bytes",
        raw.ingest
            .wal_hard_limit_bytes
            .unwrap_or(DEFAULT_WAL_HARD_LIMIT_BYTES),
        "a zero WAL ceiling cannot be satisfied by any queue depth, so the startup headroom \
         assertion could only ever refuse to start",
    )?;
    let overlay_soft_limit = non_zero_usize(
        "ingest.overlay_soft_limit",
        raw.ingest
            .overlay_soft_limit
            .unwrap_or(DEFAULT_OVERLAY_SOFT_LIMIT),
        "the alarm would fire on the very first deny and never stop; a permanently-firing alarm \
         is indistinguishable from no alarm at all",
    )?;
    let flush_max_items = non_zero_usize(
        "ingest.flush_max_items",
        raw.ingest
            .flush_max_items
            .unwrap_or(DEFAULT_FLUSH_MAX_ITEMS),
        "a zero item trigger asks flush to run before there is anything to flush (INERT this \
         stage — validated now so stage 2.2 inherits the guard rather than adding it)",
    )?;
    let flush_max_age_secs = non_zero_u64(
        "ingest.flush_max_age_secs",
        raw.ingest
            .flush_max_age_secs
            .unwrap_or(DEFAULT_FLUSH_MAX_AGE_SECS),
        "a zero age trigger asks flush to run continuously (INERT this stage — validated now so \
         stage 2.2 inherits the guard rather than adding it)",
    )?;
    let row_projection_cache_bytes = non_zero_u64(
        "serve.row_projection_cache_bytes",
        raw.serve
            .row_projection_cache_bytes
            .unwrap_or(DEFAULT_ROW_PROJECTION_CACHE_BYTES),
        "a cache that admits nothing is a 100% miss rate, and because the single-flight miss path \
         returns ProjectionBuilding to every racer that presents as a permanent 429 storm with \
         cores pegged on rebuilds, not as a slower server",
    )?;
    let fragment_cache_bytes = non_zero_u64(
        "serve.fragment_cache_bytes",
        raw.serve
            .fragment_cache_bytes
            .unwrap_or(DEFAULT_FRAGMENT_CACHE_BYTES),
        "the in-memory fragment tier would admit nothing, so every authorisation would re-open \
         and re-verify its sidecar",
    )?;
    let expected_concurrent_sessions = non_zero_usize(
        "serve.expected_concurrent_sessions",
        raw.serve
            .expected_concurrent_sessions
            .unwrap_or(DEFAULT_EXPECTED_CONCURRENT_SESSIONS),
        "the cache-admission validation would be vacuous — any cache bound, however small, \
         'admits' zero sessions",
    )?;
    let pin_ttl_secs = non_zero_u64(
        "serve.pin_ttl_secs",
        raw.serve.pin_ttl_secs.unwrap_or(DEFAULT_PIN_TTL_SECS),
        "every pin would expire the instant it was issued, so a client presenting a pin it was \
         just given gets 410 with nothing to distinguish that from a drained generation",
    )?;
    let pins_per_session_max = non_zero_usize(
        "serve.pins_per_session_max",
        raw.serve
            .pins_per_session_max
            .unwrap_or(DEFAULT_PINS_PER_SESSION_MAX),
        "no session could pin at all, silently disabling I11's pinned-geometry guarantee rather \
         than bounding it",
    )?;

    Ok(Config {
        bundle_path: raw.bundle.path,
        cache_dir: raw.bundle.cache,
        wal_path: raw.bundle.wal,
        min_visible_members,
        token_max_lifetime_secs,
        viewer_addr,
        session_addr,
        control_listen,
        max_k,
        k_min,
        k_max_marks,
        theta_target_marks,
        max_underlay_offset,
        max_underlay_cells: raw
            .serve
            .max_underlay_cells
            .unwrap_or(DEFAULT_MAX_UNDERLAY_CELLS),
        max_tiles_per_request: raw
            .serve
            .max_tiles_per_request
            .unwrap_or(DEFAULT_MAX_TILES_PER_REQUEST),
        stage_timing: raw.serve.stage_timing.unwrap_or(false),
        session_credential,
        operator_credential,
        compute_threads,
        compute_admission,
        compute_queue,
        admission_timeout_ms,
        commit_window_max_items,
        commit_window_max_age_ms,
        ingest_queue_bound,
        ingest_max_batch_rows,
        ingest_max_batch_bytes,
        wal_hard_limit_bytes,
        overlay_soft_limit,
        flush_max_items,
        flush_max_age_secs,
        row_projection_cache_bytes,
        fragment_cache_bytes,
        expected_concurrent_sessions,
        pin_ttl_secs,
        pins_per_session_max,
    })
}

fn parse_control_listen(raw: &str) -> Result<ControlListen> {
    if let Some(path) = raw.strip_prefix("unix:") {
        return Ok(ControlListen::Unix(PathBuf::from(path)));
    }
    raw.parse()
        .map(ControlListen::Tcp)
        .map_err(|_| ConfigError::BadAddr(raw.to_string()))
}

fn load_credential(name: &'static str, file: Option<&Path>, env: Option<&str>) -> Result<String> {
    if let Some(path) = file {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
    if let Some(var) = env {
        return std::env::var(var).map_err(|_| ConfigError::MissingCredential(name));
    }
    Err(ConfigError::MissingCredential(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_disclosure_section_refuses_to_start() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(err, ConfigError::MissingDisclosureSection));
    }

    #[test]
    fn missing_disclosure_key_refuses_to_start() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            min_visible_members = 10
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::MissingDisclosureKey("token_max_lifetime")
        ));
    }

    /// A complete, valid config, with `[serve]` extras interpolated — used by the selection-clause
    /// tests below so each one differs from a working config in exactly one key.
    fn valid_toml(serve_extra: &str) -> String {
        valid_toml_with(serve_extra, "")
    }

    /// [`valid_toml`] with an `[ingest]` section as well. Note the section is emitted **only**
    /// when non-empty, so `valid_toml("")` is genuinely a config with no `[ingest]` section at
    /// all — which is what `every_stage_2_1_knob_defaults` needs to be testing.
    fn valid_toml_with(serve_extra: &str, ingest_extra: &str) -> String {
        let ingest_section = if ingest_extra.is_empty() {
            String::new()
        } else {
            format!("[ingest]\n{ingest_extra}\n")
        };
        format!(
            r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            min_visible_members = 10
            token_max_lifetime = 3600
            {ingest_section}
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_SESSION_CRED"
            operator_credential_env = "TESSERA_TEST_OPERATOR_CRED"
            {serve_extra}
        "#
        )
    }

    #[test]
    fn the_selection_clauses_have_working_defaults() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.k_min, DEFAULT_K_MIN);
        assert_eq!(config.k_max_marks, DEFAULT_K_MAX_MARKS);
        assert_eq!(config.theta_target_marks, DEFAULT_THETA_TARGET_MARKS);
        assert_eq!(config.max_underlay_offset, DEFAULT_MAX_UNDERLAY_OFFSET);
        assert_eq!(config.max_underlay_cells, DEFAULT_MAX_UNDERLAY_CELLS);
        assert_eq!(config.max_k, DEFAULT_MAX_K);
        assert_eq!(config.compute_threads, default_compute_threads());
        assert_eq!(
            config.compute_admission,
            COMPUTE_ADMISSION_MULTIPLIER * config.compute_threads
        );
        assert_eq!(config.compute_queue, 2 * config.compute_admission);
        assert_eq!(config.admission_timeout_ms, DEFAULT_ADMISSION_TIMEOUT_MS);
    }

    /// D-B: `compute_admission` defaults to `COMPUTE_ADMISSION_MULTIPLIER × compute_threads`, not
    /// to a separate constant — an explicit `compute_threads` must change the default admission
    /// bound too, or the measured small-request throughput argument silently stops holding.
    #[test]
    fn compute_admission_defaults_to_compute_threads() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("compute_threads = 7")).expect("must load");
        assert_eq!(config.compute_threads, 7);
        assert_eq!(config.compute_admission, 28);
        assert_eq!(config.compute_queue, 56);
    }

    /// D-B: `compute_queue = 0` is explicitly legal — it means "shed the instant every compute
    /// permit is busy" — so it must load, not refuse.
    #[test]
    fn a_zero_compute_queue_is_legal() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml("compute_queue = 0")).expect("compute_queue = 0 must load");
        assert_eq!(config.compute_queue, 0);
    }

    /// D-B: `compute_threads = 0` refuses to start rather than silently running a zero-width
    /// pool — the same refuse-not-clamp discipline as the selection clause's `k_min = 0`.
    #[test]
    fn a_zero_compute_threads_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_threads = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ComputeThreadsZero), "{err}");
    }

    /// D-B: an explicit `compute_threads` large enough that `COMPUTE_ADMISSION_MULTIPLIER *
    /// compute_threads` overflows `usize` refuses to start rather than silently wrapping to a
    /// small, wrong `compute_admission` — release builds have overflow checks off, so an
    /// unchecked multiply would produce a bogus-but-plausible value with no error raised anywhere.
    /// `compute_threads` here is `i64::MAX` (fits TOML's integer range) so `4 * compute_threads`
    /// overflows `u64`/`usize` without overflowing on the way in from TOML itself.
    #[test]
    fn a_compute_threads_that_overflows_the_default_admission_multiply_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_threads = 9223372036854775807")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::ComputeAdmissionDefaultOverflow {
                    compute_threads: 9223372036854775807
                }
            ),
            "{err}"
        );
    }

    /// D-B: `compute_admission = 0` refuses to start — a zero-permit compute semaphore sheds
    /// every gated request unconditionally, indistinguishable from the server being down.
    #[test]
    fn a_zero_compute_admission_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("compute_admission = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ComputeAdmissionZero), "{err}");
    }

    /// D-E: `admission_timeout_ms = 0` refuses to start — that would silently disable the bounded
    /// queue wait; `compute_queue = 0` is the correct, explicit way to disable queueing.
    #[test]
    fn a_zero_admission_timeout_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("admission_timeout_ms = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::AdmissionTimeoutZero), "{err}");
    }

    /// D-B: `compute_admission + compute_queue` at an absurd (but syntactically valid) size
    /// refuses to start rather than panicking inside `Semaphore::new` during server startup —
    /// these two knobs together size the outer slots semaphore, and `Semaphore::new` panics past
    /// `MAX_PERMITS`. Both values here fit comfortably in TOML's i64 range but their sum exceeds
    /// `tokio::sync::Semaphore::MAX_PERMITS` (`usize::MAX >> 3`), without overflowing `usize`
    /// itself — the "exceeds the bound" branch, distinct from the defensive `checked_add`
    /// overflow branch that TOML's i64 ceiling makes unreachable from config alone.
    #[test]
    fn an_absurd_admission_plus_queue_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml(
            "compute_admission = 2000000000000000000\ncompute_queue = 2000000000000000000",
        ))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::ComputeAdmissionQueueOverflow { .. }),
            "{err}"
        );
    }

    /// `k_min = 0` disables §7.2's floor clause, which is the I7 guarantee. Startup must refuse
    /// rather than clamp — a clamp would mean a typo silently changed the configuration, and a
    /// pass-through would mean a typo silently disabled an invariant.
    #[test]
    fn a_zero_floor_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("k_min = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::FloorClauseDisabled), "{err}");
    }

    #[test]
    fn a_floor_above_either_cap_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        // Above the overplot ceiling.
        let err = parse(&valid_toml("k_min = 200\nk_max_marks = 128")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::FloorAboveCap {
                    k_min: 200,
                    cap_name: "k_max_marks",
                    cap: 128
                }
            ),
            "{err}"
        );

        // Above the MACHINE ceiling, which also clamps the floor — the gap the first check missed.
        let err = parse(&valid_toml("max_k = 1\nk_min = 2")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::FloorAboveCap {
                    k_min: 2,
                    cap_name: "max_k",
                    cap: 1
                }
            ),
            "{err}"
        );
    }

    /// `theta_target_marks = 0` anchors θ at a cut admitting nothing, so every tile would draw
    /// exactly `k_min` at every zoom with no error — the identical silent failure that
    /// `Threshold::at_depth`'s `leading_zeros` guard prevents, reached through config instead.
    #[test]
    fn an_underlay_offset_deeper_than_the_grid_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("max_underlay_offset = 17")).unwrap_err();
        assert!(
            matches!(err, ConfigError::UnderlayOffsetTooDeep(17)),
            "{err}"
        );
    }

    #[test]
    fn a_zero_theta_target_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("theta_target_marks = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ThetaTargetZero), "{err}");
    }

    // ---- Phase 2 stage 2.1 (Task 0b) --------------------------------------------------------

    /// **SA §7's rule, pinned as a test.** "Performance knobs default; disclosure controls do
    /// not" — and not one of the fourteen stage-2.1 knobs is a disclosure control: they size
    /// queues, windows, caches and pin lifetimes, and none of them changes what any principal may
    /// see. So a `tessera.toml` that mentions **none** of them — no `[ingest]` section at all —
    /// must load, with the documented defaults.
    ///
    /// The reading matters because the opposite reading is also plausible and is wrong: several
    /// of these knobs *bound* memory, and a reviewer who classes "bounds memory" as "must be
    /// stated explicitly" would make every deployment carry fourteen lines of boilerplate that
    /// SA §7 exists to prevent. If a later stage decides one of these really is a disclosure
    /// control, it must fail this test on the way to moving it — which is the point.
    #[test]
    fn every_stage_2_1_knob_defaults() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let toml = valid_toml("");
        assert!(
            !toml.contains("[ingest]"),
            "this test is only meaningful against a config with no [ingest] section"
        );
        let config = parse(&toml).expect("a config naming none of the stage-2.1 knobs must load");

        assert_eq!(
            config.commit_window_max_items,
            DEFAULT_COMMIT_WINDOW_MAX_ITEMS
        );
        assert_eq!(
            config.commit_window_max_age_ms,
            DEFAULT_COMMIT_WINDOW_MAX_AGE_MS
        );
        assert_eq!(config.ingest_queue_bound, DEFAULT_INGEST_QUEUE_BOUND);
        assert_eq!(config.ingest_max_batch_rows, DEFAULT_INGEST_MAX_BATCH_ROWS);
        assert_eq!(
            config.ingest_max_batch_bytes,
            DEFAULT_INGEST_MAX_BATCH_BYTES
        );
        assert_eq!(config.wal_hard_limit_bytes, DEFAULT_WAL_HARD_LIMIT_BYTES);
        assert_eq!(config.overlay_soft_limit, DEFAULT_OVERLAY_SOFT_LIMIT);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(
            config.row_projection_cache_bytes,
            DEFAULT_ROW_PROJECTION_CACHE_BYTES
        );
        assert_eq!(config.fragment_cache_bytes, DEFAULT_FRAGMENT_CACHE_BYTES);
        assert_eq!(
            config.expected_concurrent_sessions,
            DEFAULT_EXPECTED_CONCURRENT_SESSIONS
        );
        assert_eq!(config.pin_ttl_secs, DEFAULT_PIN_TTL_SECS);
        assert_eq!(config.pins_per_session_max, DEFAULT_PINS_PER_SESSION_MAX);
    }

    /// And the keys are actually wired to their fields — a defaults test alone would pass just as
    /// happily against fourteen constants nothing parses. One key per section, plus the byte- and
    /// duration-typed shapes, so a mis-sectioned or mis-typed key is caught here.
    #[test]
    fn the_stage_2_1_knobs_are_read_from_their_sections() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let config = parse(&valid_toml_with(
            "pin_ttl_secs = 42\nrow_projection_cache_bytes = 777000000",
            "commit_window_max_items = 7\nwal_hard_limit_bytes = 999000000",
        ))
        .expect("must load");
        assert_eq!(config.pin_ttl_secs, 42);
        assert_eq!(config.row_projection_cache_bytes, 777_000_000);
        assert_eq!(config.commit_window_max_items, 7);
        assert_eq!(config.wal_hard_limit_bytes, 999_000_000);
    }

    /// Every stage-2.1 knob refuses a zero, and refuses it by *name*. Zero is degenerate for all
    /// fourteen — never "off" — and the failure modes are silent ones: a window that batches
    /// nothing, a queue that blocks instead of shedding, an alarm that never stops firing, a
    /// cache that turns every request into a 429. Same discipline as `k_min = 0`.
    ///
    /// Table-driven deliberately: a knob added to `Config` without a zero check will not show up
    /// here as a compile error, so the list is also the checklist a reviewer reads against the
    /// struct.
    #[test]
    fn a_zero_stage_2_1_knob_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");

        let ingest_keys = [
            "commit_window_max_items",
            "commit_window_max_age_ms",
            "ingest_queue_bound",
            "ingest_max_batch_rows",
            "ingest_max_batch_bytes",
            "wal_hard_limit_bytes",
            "overlay_soft_limit",
            "flush_max_items",
            "flush_max_age_secs",
        ];
        let serve_keys = [
            "row_projection_cache_bytes",
            "fragment_cache_bytes",
            "expected_concurrent_sessions",
            "pin_ttl_secs",
            "pins_per_session_max",
        ];
        assert_eq!(
            ingest_keys.len() + serve_keys.len(),
            14,
            "the plan lands fourteen keys; this table must cover all of them"
        );

        for key in ingest_keys {
            let err = parse(&valid_toml_with("", &format!("{key} = 0"))).unwrap_err();
            let ConfigError::MustBeNonZero { key: named, .. } = err else {
                panic!("ingest.{key} = 0 must be refused as MustBeNonZero, got {err}");
            };
            assert_eq!(named, format!("ingest.{key}"));
        }
        for key in serve_keys {
            let err = parse(&valid_toml(&format!("{key} = 0"))).unwrap_err();
            let ConfigError::MustBeNonZero { key: named, .. } = err else {
                panic!("serve.{key} = 0 must be refused as MustBeNonZero, got {err}");
            };
            assert_eq!(named, format!("serve.{key}"));
        }
    }

    /// **The inertness assertion.** `flush_max_items` and `flush_max_age_secs` are parsed,
    /// validated and stored, and *nothing reads them* — their consumer is flush, which does not
    /// exist until stage 2.2. An operator must not be able to set one and believe it works, so
    /// the claim is checked mechanically rather than promised in a doc comment: no `.rs` file in
    /// the workspace outside this module may name either key.
    ///
    /// When stage 2.2 wires flush, this test fails. That is the intended design: the failure is
    /// the prompt to delete the INERT paragraphs from both `DEFAULT_FLUSH_*` constants and both
    /// `Config` fields in the same commit that gives them a consumer. Deleting this test without
    /// doing that is the failure mode it exists to prevent, so it says so here.
    #[test]
    fn the_flush_knobs_are_inert() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/<crate> always has two ancestors")
            .to_path_buf();
        let crates = workspace.join("crates");
        assert!(
            crates.is_dir(),
            "cannot scan for consumers: {} is not a directory",
            crates.display()
        );
        // This module is the one legitimate mention of both keys.
        let this_file = workspace.join("crates/tessera-server/src/config.rs");
        assert!(this_file.is_file(), "{} moved", this_file.display());

        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("readable crates tree") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    // `target/` can appear inside a crate dir; nothing generated is a consumer.
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && path != this_file {
                    let text = fs::read_to_string(&path).expect("readable source file");
                    if text.contains("flush_max_items") || text.contains("flush_max_age_secs") {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "flush_max_items / flush_max_age_secs are documented as INERT until stage 2.2, but \
             are named in: {offenders:?}. If flush now consumes them, delete this test AND the \
             INERT paragraphs on DEFAULT_FLUSH_MAX_ITEMS, DEFAULT_FLUSH_MAX_AGE_SECS and both \
             Config fields — an operator reading a stale 'INERT' note is exactly what this test \
             prevents"
        );
    }

    /// The defaults are a *consistent set*, not fourteen independent numbers. Task 6 asserts at
    /// startup that `ingest_queue_bound × ingest_max_batch_bytes`, plus reserved deny headroom,
    /// sits strictly below `wal_hard_limit_bytes`. That assertion does not exist yet — but if the
    /// defaults do not satisfy it, the *default* configuration will refuse to start the day it
    /// lands, which is not a default. Checked here so a later edit to one constant cannot break
    /// the relation silently.
    #[test]
    fn defaults_satisfy_task_6s_headroom_relation() {
        let queue_worst_case =
            DEFAULT_INGEST_QUEUE_BOUND as u64 * DEFAULT_INGEST_MAX_BATCH_BYTES as u64;
        assert!(
            queue_worst_case < DEFAULT_WAL_HARD_LIMIT_BYTES,
            "queue worst case {queue_worst_case} B does not fit under the WAL ceiling \
             {DEFAULT_WAL_HARD_LIMIT_BYTES} B"
        );
        // And with room to spare, because denies are never shed for load and so have no
        // admission control of their own to fall back on when the ingest queue is full. Task 6
        // chooses the reserved figure; this asserts the shape of the choice — the queue may not
        // be able to consume even half the ceiling.
        assert!(
            queue_worst_case * 2 < DEFAULT_WAL_HARD_LIMIT_BYTES,
            "a full ingest queue may not be able to claim half the WAL ceiling; denies need \
             headroom above it"
        );
    }

    /// The other cross-knob relation, for the same reason. Task 5 refuses to start unless
    /// `row_projection_cache_bytes` admits at least `expected_concurrent_sessions` entries at the
    /// measured per-entry size — so the defaults must admit that many, with the ~2× margin the
    /// serialised-size accounting's own underestimate calls for.
    #[test]
    fn defaults_satisfy_task_5s_cache_relation() {
        let working_set =
            DEFAULT_EXPECTED_CONCURRENT_SESSIONS as u64 * MEASURED_PROJECTION_BYTES_AT_1E9;
        assert!(
            DEFAULT_ROW_PROJECTION_CACHE_BYTES >= working_set,
            "{DEFAULT_ROW_PROJECTION_CACHE_BYTES} B admits fewer than \
             {DEFAULT_EXPECTED_CONCURRENT_SESSIONS} projections of \
             {MEASURED_PROJECTION_BYTES_AT_1E9} B"
        );
        assert!(
            DEFAULT_ROW_PROJECTION_CACHE_BYTES >= 2 * working_set,
            "the bound must carry the ~2× margin for the in-memory footprint that \
             get_serialized_size_in_bytes underestimates"
        );
    }
}
