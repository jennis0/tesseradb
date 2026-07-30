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
    /// Emit `x-tessera-stage-ns` on viewport responses. **Fails closed**: absent means false, and
    /// even true does nothing in a binary built without the `bench-timing` feature. The header
    /// carries only durations and row counts — no identifier, no per-principal label (SA §9) —
    /// but it quantifies the C4 timing channel, so it stays off unless a measurement asked for it.
    pub stage_timing: bool,
    pub session_credential: String,
    pub operator_credential: String,
}

/// Reference Sheet R1: viewport `k`'s cap default.
const DEFAULT_MAX_K: usize = 200;

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

    Ok(Config {
        bundle_path: raw.bundle.path,
        cache_dir: raw.bundle.cache,
        wal_path: raw.bundle.wal,
        min_visible_members,
        token_max_lifetime_secs,
        viewer_addr,
        session_addr,
        control_listen,
        max_k: raw.serve.max_k.unwrap_or(DEFAULT_MAX_K),
        stage_timing: raw.serve.stage_timing.unwrap_or(false),
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
}
