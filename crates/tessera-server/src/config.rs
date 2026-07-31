//! Fail-closed configuration: `tessera.toml` (SA §7).
//!
//! `[disclosure]` has no defaults at all: absence of the section, or of either key inside it, is
//! a startup error naming design §7.5/§2.3 — `min_visible_members` is parsed and stored even
//! though nothing consumes it until Phase 3; the startup rule, not the value, is the point.
//! Every other section either has a documented default (`max_k = 1000`) or is required outright.
//! Credentials are never inline: `[serve]`'s `*_credential_file`/`*_credential_env` pairs are the
//! only way to supply the session/operator bearer secrets.

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
    ComputeAdmissionDefaultOverflow { compute_threads: usize },
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
}
