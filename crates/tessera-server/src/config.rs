//! Fail-closed configuration: `tessera.toml` (SA §7).
//!
//! `[disclosure]` has no defaults at all: absence of the section, or of either key inside it, is
//! a startup error naming design §7.5/§2.3 — `min_visible_members` is parsed and stored even
//! though nothing consumes it until Phase 3; the startup rule, not the value, is the point.
//! Every other section either has a documented default (`max_k = 200`) or is required outright.
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
    /// `serve.theta_target_marks = 0`, which anchors θ at `Cut(0)` — a threshold that admits
    /// **nothing**, at every depth, because `0u64.leading_zeros() == 64` so the per-depth shift
    /// always "fits". Every non-empty tile would then draw exactly `k_min` marks at every zoom
    /// forever, with no error raised anywhere: design §7.2's density signal silently gone.
    ///
    /// This is the *same* silent failure mode `Threshold::at_depth`'s `leading_zeros` check exists to
    /// prevent, reachable through config instead of through a shift bug — so it is refused in the
    /// same spirit.
    ThetaTargetZero,
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
            ConfigError::ThetaTargetZero => write!(
                f,
                "serve.theta_target_marks = 0 anchors design §7.2's threshold at a cut that admits \
                 nothing, at every depth — so every non-empty tile would draw exactly k_min marks \
                 at every zoom, with the density signal silently gone. Startup refuses rather than \
                 serving a map that looks plausible and conveys nothing"
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
    pub session_credential: String,
    pub operator_credential: String,
}

/// Reference Sheet R1: viewport `k`'s cap default. **The machine ceiling** — the drawn-mark budget
/// spec's probes calibrate this number, and that calibration is blocked on the external-ID identity
/// plan's rebuild, so this value must not be changed here.
const DEFAULT_MAX_K: usize = 200;

/// §7.2's floor clause: the fewest marks a non-empty tile draws. Provisional (density memo §4),
/// pending that memo's §0 visual experiments.
const DEFAULT_K_MIN: usize = 2;

/// §7.2's cap clause: the most marks any one tile draws. **The overplot ceiling** — density memo §4
/// sizes it from ink coverage at ~80x80 px per tile, not from machine limits. Provisional.
const DEFAULT_K_MAX_MARKS: usize = 128;

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
        max_underlay_offset: raw
            .serve
            .max_underlay_offset
            .unwrap_or(DEFAULT_MAX_UNDERLAY_OFFSET),
        max_underlay_cells: raw
            .serve
            .max_underlay_cells
            .unwrap_or(DEFAULT_MAX_UNDERLAY_CELLS),
        max_tiles_per_request: raw
            .serve
            .max_tiles_per_request
            .unwrap_or(DEFAULT_MAX_TILES_PER_REQUEST),
        session_credential,
        operator_credential,
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
        assert_eq!(
            config.max_k, DEFAULT_MAX_K,
            "max_k is the machine ceiling and is not this plan's to change"
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
    fn a_zero_theta_target_refuses_to_start() {
        std::env::set_var("TESSERA_TEST_SESSION_CRED", "s");
        std::env::set_var("TESSERA_TEST_OPERATOR_CRED", "o");
        let err = parse(&valid_toml("theta_target_marks = 0")).unwrap_err();
        assert!(matches!(err, ConfigError::ThetaTargetZero), "{err}");
    }
}
