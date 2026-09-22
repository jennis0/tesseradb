//! `tessera.toml`: what a deployment is, read by both `tessera build` and `tessera serve`.
//!
//! The file is found by walking up from the working directory ([`discover`]), and every path in
//! it resolves against its own directory ([`load`]). The serving secrets are located here and read
//! at startup ([`Credential::resolve`]), so a build never needs them. Every raw section refuses a
//! key it does not know.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::sync::Semaphore;

pub mod defaults;
mod error;

use defaults::*;
pub use error::ConfigError;

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    bundle: RawBundle,
    plugin: RawPlugin,
    disclosure: Option<RawDisclosure>,
    #[serde(default)]
    build: RawBuild,
    /// Read by hand, so that `key = "…"` gets its own refusal.
    identity: Option<toml::Value>,
    #[serde(default)]
    serve: RawServe,
    #[serde(default)]
    ingest: RawIngest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisclosure {
    token_max_lifetime: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIngest {
    commit_window_max_items: Option<usize>,
    ingest_queue_bound: Option<usize>,
    ingest_admission: Option<usize>,
    ingest_max_batch_rows: Option<usize>,
    ingest_max_batch_bytes: Option<usize>,
    publish_max_body_bytes: Option<usize>,
    max_artifacts_per_request: Option<usize>,
    max_members_per_request: Option<usize>,
    max_excluded_per_request: Option<usize>,
    overlay_soft_limit: Option<usize>,
    flush_max_age_secs: Option<u64>,
    flush_max_items: Option<usize>,
    ingest_buffer_max_items: Option<usize>,
    compaction_min_interval_secs: Option<u64>,
    compaction_window_start: Option<String>,
    compaction_window_secs: Option<u32>,
    compaction_window_min_segments: Option<usize>,
    compaction_max_segments: Option<OrOff<u64>>,
    compaction_after_deletions: Option<OrOff<u64>>,
    compaction_dead_rows_fraction: Option<OrOff<f64>>,
    compaction_dead_bytes_ratio: Option<OrOff<f64>>,
}

/// A number, or the word `"off"`. Any other word is refused by [`or_off`].
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum OrOff<T> {
    Value(T),
    Word(String),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    schema: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundle {
    path: PathBuf,
    cache: PathBuf,
    wal: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    module: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    viewer: Option<String>,
    session: Option<String>,
    control: Option<String>,
    max_k: Option<usize>,
    stage_timing: Option<bool>,
    k_min: Option<usize>,
    k_max_marks: Option<usize>,
    theta_target_marks: Option<u64>,
    max_underlay_offset: Option<u8>,
    max_underlay_cells: Option<usize>,
    max_tiles_per_request: Option<usize>,
    max_category_values: Option<usize>,
    max_suggestions: Option<usize>,
    max_suggestion_walk: Option<u64>,
    max_suggest_set_entities: Option<u64>,
    max_shape_vertices: Option<u64>,
    max_region_vertices: Option<u64>,
    max_region_cells: Option<usize>,
    max_browse_rows: Option<usize>,
    region_cache_bytes: Option<u64>,
    session_credential_file: Option<PathBuf>,
    session_credential_env: Option<String>,
    operator_credential_file: Option<PathBuf>,
    operator_credential_env: Option<String>,
    compute_threads: Option<usize>,
    compute_admission: Option<usize>,
    compute_queue: Option<usize>,
    admission_timeout_ms: Option<u64>,
    single_flight_wait_ms: Option<u64>,
    stream_flush_bytes: Option<usize>,
    stream_write_stall_ms: Option<u64>,
    stream_deadline_ms: Option<u64>,
    row_projection_cache_bytes: Option<u64>,
    masked_count_cache_bytes: Option<u64>,
    occupancy_cache_bytes: Option<u64>,
    fragment_cache_bytes: Option<u64>,
    segment_floor_bytes: Option<u64>,
    max_merged_segment_bytes: Option<u64>,
    tier_width: Option<usize>,
    coalesce_width: Option<usize>,
    dev_cors_origins: Option<Vec<String>>,
    cors_origins: Option<Vec<String>>,
    cors_loopback: Option<bool>,
    visible_wait_max_secs: Option<u64>,
}

/// The control plane's listen target: a unix socket, or a loopback TCP address.
#[derive(Debug, Clone)]
pub enum ControlListen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where `tessera build` writes and `tessera serve` opens; `tessera build --out` overrides it.
    pub bundle_path: PathBuf,
    pub cache_dir: PathBuf,
    pub wal_path: PathBuf,
    /// The corpus declaration `tessera build` reads. The server reads its schema from the bundle.
    pub schema_path: PathBuf,
    /// The name of the environment variable holding the identity key, never the key.
    pub identity_env: String,
    pub token_max_lifetime_secs: u64,
    /// `None` when the file declares no `[serve]` addresses; `prepare` refuses to serve then.
    pub viewer_addr: Option<SocketAddr>,
    pub session_addr: Option<SocketAddr>,
    pub control_listen: Option<ControlListen>,
    pub max_k: usize,
    pub k_min: usize,
    pub k_max_marks: usize,
    pub theta_target_marks: u64,
    pub max_underlay_offset: u8,
    pub max_underlay_cells: usize,
    pub max_tiles_per_request: usize,
    pub max_category_values: usize,
    pub max_suggestions: usize,
    pub max_suggestion_walk: u64,
    pub max_suggest_set_entities: u64,
    pub max_shape_vertices: u64,
    pub max_region_vertices: u64,
    pub max_region_cells: usize,
    pub max_browse_rows: usize,
    pub region_cache_bytes: u64,
    /// Emit `x-tessera-stage-ns`; does nothing in a binary built without `bench-timing`.
    pub stage_timing: bool,
    pub dev_cors_origins: Vec<String>,
    pub cors_origins: Vec<String>,
    pub visible_wait_max_secs: u64,
    pub cors_loopback: bool,
    pub session_credential: Credential,
    pub operator_credential: Credential,
    pub compute_threads: usize,
    pub compute_admission: usize,
    pub compute_queue: usize,
    pub admission_timeout_ms: u64,
    pub single_flight_wait_ms: u64,
    pub stream_flush_bytes: usize,
    pub stream_write_stall_ms: u64,
    pub stream_deadline_ms: u64,
    /// In rows.
    pub commit_window_max_items: usize,
    pub ingest_queue_bound: usize,
    pub ingest_admission: usize,
    pub ingest_max_batch_rows: usize,
    pub ingest_max_batch_bytes: usize,
    pub publish_max_body_bytes: usize,
    pub max_artifacts_per_request: usize,
    pub max_members_per_request: usize,
    /// Published on `/control/status`; the field it bounds is not built yet.
    pub max_excluded_per_request: usize,
    pub overlay_soft_limit: usize,
    pub compaction: tessera_engine::CompactionSchedule,
    pub flush_max_age_secs: u64,
    pub flush_max_items: usize,
    pub ingest_buffer_max_items: usize,
    pub segment_floor_bytes: u64,
    /// `None` keeps the engine's own default.
    pub max_merged_segment_bytes: Option<u64>,
    pub tier_width: usize,
    pub coalesce_width: usize,
    pub row_projection_cache_bytes: u64,
    pub masked_count_cache_bytes: u64,
    pub occupancy_cache_bytes: u64,
    pub fragment_cache_bytes: u64,
}

/// The serving runtime's `max_blocking_threads`: every viewer and ingest request that admission
/// lets through holds one blocking thread, plus [`BLOCKING_THREAD_RESERVE`]. A queued request
/// holds none, so `compute_queue` is not a term.
pub fn serving_blocking_threads(config: &Config) -> usize {
    config
        .compute_admission
        .saturating_add(config.ingest_admission)
        .saturating_add(BLOCKING_THREAD_RESERVE)
}

/// `HH:MM`, 24-hour, to seconds past UTC midnight. Two digits each, hour below 24, minute below 60.
fn parse_time_of_day(value: &str) -> Result<u32> {
    let bad = || ConfigError::CompactionWindowNotATime(value.to_string());
    let (hh, mm) = value.split_once(':').ok_or_else(bad)?;
    if hh.len() != 2 || mm.len() != 2 {
        return Err(bad());
    }
    let hour: u32 = hh.parse().map_err(|_| bad())?;
    let minute: u32 = mm.parse().map_err(|_| bad())?;
    if hour > 23 || minute > 59 {
        return Err(bad());
    }
    Ok(hour * 3_600 + minute * 60)
}

/// Absent takes `default`, `"off"` is `None`, and any other word is refused naming `key`.
fn or_off<T: Copy>(key: &'static str, raw: Option<&OrOff<T>>, default: T) -> Result<Option<T>> {
    match raw {
        None => Ok(Some(default)),
        Some(OrOff::Value(value)) => Ok(Some(*value)),
        Some(OrOff::Word(word)) if word == "off" => Ok(None),
        Some(OrOff::Word(word)) => Err(ConfigError::NotANumberOrOff {
            key,
            value: word.clone(),
        }),
    }
}

/// The file name every deployment's configuration is found under.
pub const DEPLOYMENT_FILE: &str = "tessera.toml";

/// The environment variable holding the identity key when `[identity]` names none.
pub const DEFAULT_IDENTITY_ENV: &str = "TESSERA_IDENTITY_KEY";

/// The corpus declaration when `[build]` names none.
pub const DEFAULT_SCHEMA_FILE: &str = "schema.toml";

/// This deployment's `tessera.toml`: `explicit` if given, else the nearest one at or above `from`.
pub fn discover(explicit: Option<&Path>, from: &Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let mut dir = Some(from);
    while let Some(here) = dir {
        let candidate = here.join(DEPLOYMENT_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = here.parent();
    }
    Err(ConfigError::NoDeploymentConfig {
        from: from.to_path_buf(),
    })
}

/// Parse the file at `path`, resolving every relative path in it against the file's directory.
pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)?;
    let mut config = parse(&text)?;
    let base = path.parent().unwrap_or(Path::new(""));
    for slot in [
        Some(&mut config.bundle_path),
        Some(&mut config.cache_dir),
        Some(&mut config.wal_path),
        Some(&mut config.schema_path),
        config.session_credential.file.as_mut(),
        config.operator_credential.file.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        if slot.is_relative() {
            *slot = base.join(&*slot);
        }
    }
    Ok(config)
}

/// [`discover`] then [`load`], with the path the deployment was found at.
pub fn open(explicit: Option<&Path>, from: &Path) -> std::result::Result<(PathBuf, Config), String> {
    let path = discover(explicit, from).map_err(|e| e.to_string())?;
    let config = load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, config))
}

fn parse(text: &str) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;

    if raw.plugin.module != "builtin:passthrough" {
        return Err(ConfigError::UnsupportedPlugin(raw.plugin.module));
    }

    let token_max_lifetime_secs = raw
        .disclosure
        .ok_or(ConfigError::MissingDisclosureSection)?
        .token_max_lifetime
        .ok_or(ConfigError::MissingDisclosureKey("token_max_lifetime"))?;

    let identity_env = match &raw.identity {
        None => DEFAULT_IDENTITY_ENV.to_string(),
        Some(value) => {
            let table = value.as_table().ok_or(ConfigError::IdentityNotATable)?;
            for key in table.keys() {
                match key.as_str() {
                    "env" => {}
                    "key" => return Err(ConfigError::IdentityKeyInline),
                    other => return Err(ConfigError::UnknownIdentityKey(other.to_string())),
                }
            }
            match table.get("env").and_then(toml::Value::as_str) {
                Some(name) if !name.trim().is_empty() => name.to_string(),
                _ => DEFAULT_IDENTITY_ENV.to_string(),
            }
        }
    };
    let schema_path = raw
        .build
        .schema
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SCHEMA_FILE));

    let serve = raw.serve;
    let socket = |key: &'static str, value: &Option<String>| {
        value
            .as_deref()
            .map(|a| {
                a.parse().map_err(|_| ConfigError::BadAddr {
                    key,
                    value: a.to_string(),
                })
            })
            .transpose()
    };
    let viewer_addr = socket("viewer", &serve.viewer)?;
    let session_addr = socket("session", &serve.session)?;
    let control_listen = serve
        .control
        .as_deref()
        .map(parse_control_listen)
        .transpose()?;

    let dev_cors_origins = serve.dev_cors_origins.unwrap_or_default();
    let cors_origins = serve.cors_origins.unwrap_or_default();
    for (key, origins) in [
        ("dev_cors_origins", &dev_cors_origins),
        ("cors_origins", &cors_origins),
    ] {
        if origins.iter().any(|origin| origin.trim() == "*") {
            return Err(ConfigError::CorsWildcard { key });
        }
    }

    let compute_threads = serve
        .compute_threads
        .unwrap_or_else(tessera_engine::default_compute_threads);
    let compute_admission = serve
        .compute_admission
        .unwrap_or(compute_threads.saturating_mul(COMPUTE_ADMISSION_MULTIPLIER));
    let compute_queue = serve
        .compute_queue
        .unwrap_or(compute_admission.saturating_mul(2));
    if compute_admission
        .checked_add(compute_queue)
        .is_none_or(|slots| slots > Semaphore::MAX_PERMITS)
    {
        return Err(ConfigError::AdmissionTooLarge {
            key: "serve.compute_admission + serve.compute_queue",
        });
    }
    let ingest = raw.ingest;
    let ingest_admission = ingest
        .ingest_admission
        .unwrap_or(DEFAULT_INGEST_ADMISSION);
    if ingest_admission > Semaphore::MAX_PERMITS {
        return Err(ConfigError::AdmissionTooLarge {
            key: "ingest.ingest_admission",
        });
    }

    let overlay_soft_limit = ingest
        .overlay_soft_limit
        .unwrap_or(DEFAULT_OVERLAY_SOFT_LIMIT);
    let compaction = tessera_engine::CompactionSchedule {
        min_interval_secs: ingest
            .compaction_min_interval_secs
            .unwrap_or(DEFAULT_COMPACTION_MIN_INTERVAL_SECS),
        window_start_secs: match ingest.compaction_window_start.as_deref() {
            None => Some(DEFAULT_COMPACTION_WINDOW_START_SECS),
            Some("off") => None,
            Some(value) => Some(parse_time_of_day(value)?),
        },
        window_secs: ingest
            .compaction_window_secs
            .unwrap_or(DEFAULT_COMPACTION_WINDOW_SECS),
        window_min_segments: ingest
            .compaction_window_min_segments
            .unwrap_or(DEFAULT_COMPACTION_WINDOW_MIN_SEGMENTS),
        max_segments: or_off(
            "ingest.compaction_max_segments",
            ingest.compaction_max_segments.as_ref(),
            DEFAULT_COMPACTION_MAX_SEGMENTS,
        )?
        .map(|n| n as usize),
        after_deletions: or_off(
            "ingest.compaction_after_deletions",
            ingest.compaction_after_deletions.as_ref(),
            overlay_soft_limit as u64,
        )?,
        tombstoned_rows_fraction: or_off(
            "ingest.compaction_dead_rows_fraction",
            ingest.compaction_dead_rows_fraction.as_ref(),
            DEFAULT_COMPACTION_DEAD_ROWS_FRACTION,
        )?,
        dead_bytes_ratio: or_off(
            "ingest.compaction_dead_bytes_ratio",
            ingest.compaction_dead_bytes_ratio.as_ref(),
            DEFAULT_COMPACTION_DEAD_BYTES_RATIO,
        )?,
    };

    Ok(Config {
        bundle_path: raw.bundle.path,
        cache_dir: raw.bundle.cache,
        wal_path: raw.bundle.wal,
        schema_path,
        identity_env,
        token_max_lifetime_secs,
        viewer_addr,
        session_addr,
        control_listen,
        max_k: serve.max_k.unwrap_or(DEFAULT_MAX_K),
        k_min: serve.k_min.unwrap_or(DEFAULT_K_MIN),
        k_max_marks: serve.k_max_marks.unwrap_or(DEFAULT_K_MAX_MARKS),
        theta_target_marks: serve
            .theta_target_marks
            .unwrap_or(DEFAULT_THETA_TARGET_MARKS),
        max_underlay_offset: serve
            .max_underlay_offset
            .unwrap_or(DEFAULT_MAX_UNDERLAY_OFFSET),
        max_underlay_cells: serve
            .max_underlay_cells
            .unwrap_or(DEFAULT_MAX_UNDERLAY_CELLS),
        max_tiles_per_request: serve
            .max_tiles_per_request
            .unwrap_or(DEFAULT_MAX_TILES_PER_REQUEST),
        max_category_values: serve
            .max_category_values
            .unwrap_or(DEFAULT_MAX_CATEGORY_VALUES),
        max_suggestions: serve.max_suggestions.unwrap_or(DEFAULT_MAX_SUGGESTIONS),
        max_suggestion_walk: serve
            .max_suggestion_walk
            .unwrap_or(DEFAULT_MAX_SUGGESTION_WALK),
        max_suggest_set_entities: serve
            .max_suggest_set_entities
            .unwrap_or(DEFAULT_MAX_SUGGEST_SET_ENTITIES),
        max_shape_vertices: serve
            .max_shape_vertices
            .unwrap_or(tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES),
        max_region_vertices: serve
            .max_region_vertices
            .unwrap_or(DEFAULT_MAX_REGION_VERTICES),
        max_region_cells: serve
            .max_region_cells
            .unwrap_or(tessera_engine::DEFAULT_MAX_REGION_CELLS),
        max_browse_rows: serve.max_browse_rows.unwrap_or(DEFAULT_MAX_BROWSE_ROWS),
        region_cache_bytes: serve
            .region_cache_bytes
            .unwrap_or(DEFAULT_REGION_CACHE_BYTES),
        stage_timing: serve.stage_timing.unwrap_or(false),
        dev_cors_origins,
        cors_origins,
        visible_wait_max_secs: serve
            .visible_wait_max_secs
            .unwrap_or(DEFAULT_VISIBLE_WAIT_MAX_SECS),
        cors_loopback: serve.cors_loopback.unwrap_or(false),
        session_credential: Credential {
            file: serve.session_credential_file,
            env: serve.session_credential_env,
        },
        operator_credential: Credential {
            file: serve.operator_credential_file,
            env: serve.operator_credential_env,
        },
        compute_threads,
        compute_admission,
        compute_queue,
        admission_timeout_ms: serve
            .admission_timeout_ms
            .unwrap_or(DEFAULT_ADMISSION_TIMEOUT_MS),
        single_flight_wait_ms: serve
            .single_flight_wait_ms
            .unwrap_or(tessera_engine::DEFAULT_SINGLE_FLIGHT_WAIT_MS),
        stream_flush_bytes: serve
            .stream_flush_bytes
            .unwrap_or(DEFAULT_STREAM_FLUSH_BYTES),
        stream_write_stall_ms: serve
            .stream_write_stall_ms
            .unwrap_or(DEFAULT_STREAM_WRITE_STALL_MS),
        stream_deadline_ms: serve
            .stream_deadline_ms
            .unwrap_or(DEFAULT_STREAM_DEADLINE_MS),
        commit_window_max_items: ingest
            .commit_window_max_items
            .unwrap_or(DEFAULT_COMMIT_WINDOW_MAX_ITEMS),
        ingest_queue_bound: ingest
            .ingest_queue_bound
            .unwrap_or(DEFAULT_INGEST_QUEUE_BOUND),
        ingest_admission,
        ingest_max_batch_rows: ingest
            .ingest_max_batch_rows
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_ROWS),
        ingest_max_batch_bytes: ingest
            .ingest_max_batch_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BATCH_BYTES),
        publish_max_body_bytes: ingest
            .publish_max_body_bytes
            .unwrap_or(DEFAULT_PUBLISH_MAX_BODY_BYTES),
        max_artifacts_per_request: ingest
            .max_artifacts_per_request
            .unwrap_or(DEFAULT_MAX_ARTIFACTS_PER_REQUEST),
        max_members_per_request: ingest
            .max_members_per_request
            .unwrap_or(DEFAULT_MAX_MEMBERS_PER_REQUEST),
        max_excluded_per_request: ingest
            .max_excluded_per_request
            .unwrap_or(DEFAULT_MAX_EXCLUDED_PER_REQUEST),
        overlay_soft_limit,
        compaction,
        flush_max_age_secs: ingest
            .flush_max_age_secs
            .unwrap_or(DEFAULT_FLUSH_MAX_AGE_SECS),
        flush_max_items: ingest.flush_max_items.unwrap_or(DEFAULT_FLUSH_MAX_ITEMS),
        ingest_buffer_max_items: ingest
            .ingest_buffer_max_items
            .unwrap_or(DEFAULT_INGEST_BUFFER_MAX_ITEMS),
        segment_floor_bytes: serve
            .segment_floor_bytes
            .unwrap_or(DEFAULT_SEGMENT_FLOOR_BYTES),
        max_merged_segment_bytes: serve.max_merged_segment_bytes,
        tier_width: serve.tier_width.unwrap_or(DEFAULT_TIER_WIDTH),
        coalesce_width: serve.coalesce_width.unwrap_or(DEFAULT_COALESCE_WIDTH),
        row_projection_cache_bytes: serve
            .row_projection_cache_bytes
            .unwrap_or(DEFAULT_ROW_PROJECTION_CACHE_BYTES),
        masked_count_cache_bytes: serve
            .masked_count_cache_bytes
            .unwrap_or(DEFAULT_MASKED_COUNT_CACHE_BYTES),
        occupancy_cache_bytes: serve
            .occupancy_cache_bytes
            .unwrap_or(tessera_engine::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES),
        fragment_cache_bytes: serve
            .fragment_cache_bytes
            .unwrap_or(DEFAULT_FRAGMENT_CACHE_BYTES),
    })
}

fn parse_control_listen(raw: &str) -> Result<ControlListen> {
    if let Some(path) = raw.strip_prefix("unix:") {
        return Ok(ControlListen::Unix(PathBuf::from(path)));
    }
    raw.parse()
        .map(ControlListen::Tcp)
        .map_err(|_| ConfigError::BadAddr {
            key: "control",
            value: raw.to_string(),
        })
}

/// Where a bearer secret is: a file, or an environment variable. The secret itself is read only
/// by [`Credential::resolve`], so it never sits in a `Debug`-printed [`Config`].
#[derive(Debug, Clone)]
pub struct Credential {
    file: Option<PathBuf>,
    env: Option<String>,
}

impl Credential {
    /// Read the secret, or refuse a credential that is declared nowhere or cannot be read.
    pub fn resolve(&self, name: &'static str) -> Result<String> {
        if let Some(path) = &self.file {
            let secret = fs::read_to_string(path).map_err(|source| {
                ConfigError::CredentialFileUnreadable {
                    which: name,
                    path: path.clone(),
                    source,
                }
            })?;
            return Ok(secret.trim().to_string());
        }
        if let Some(var) = &self.env {
            return std::env::var(var).map_err(|_| ConfigError::MissingCredential(name));
        }
        Err(ConfigError::MissingCredential(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete config with `[serve]` extras interpolated, so each case differs from a working
    /// config in the keys it names.
    fn valid_toml(serve_extra: &str) -> String {
        valid_toml_with(serve_extra, "")
    }

    /// [`valid_toml`] with an `[ingest]` section, emitted only when `ingest_extra` is non-empty.
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

    /// `tessera build` reads this file too, so a deployment with no `[serve]` section parses.
    #[test]
    fn a_build_only_deployment_needs_no_serve_section() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            token_max_lifetime = 3600
        "#;
        let config = parse(toml).expect("a build-only deployment parses");
        assert!(config.viewer_addr.is_none());
        assert!(config.session_addr.is_none());
        assert!(config.control_listen.is_none());
    }

    #[test]
    fn missing_disclosure_section_refuses_to_start() {
        let toml = r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
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
        "#;
        let err = parse(toml).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::MissingDisclosureKey("token_max_lifetime")
        ));
    }

    #[test]
    fn an_unknown_disclosure_key_refuses_to_start() {
        let with = |extra: &str| {
            format!(
                r#"
                [bundle]
                path = "b"
                cache = "c"
                wal = "w"
                [plugin]
                module = "builtin:passthrough"
                [disclosure]
                token_max_lifetime = 3600
                {extra}
            "#
            )
        };
        parse(&with("")).expect("token_max_lifetime alone is the whole section");
        let err = parse(&with("min_visible_members = 10")).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    #[test]
    fn the_compaction_schedule_defaults_to_section_9s_own_numbers() {
        let config = parse(&valid_toml("")).unwrap();
        assert_eq!(
            config.compaction,
            tessera_engine::CompactionSchedule {
                min_interval_secs: 86_400,
                window_start_secs: Some(0),
                window_secs: 4 * 3_600,
                window_min_segments: 8,
                max_segments: Some(64),
                after_deletions: Some(DEFAULT_OVERLAY_SOFT_LIMIT as u64),
                tombstoned_rows_fraction: Some(0.2),
                dead_bytes_ratio: Some(1.0),
            }
        );
    }

    /// A ratio gauge takes any number or `"off"`, and refuses any other word by name.
    #[test]
    fn a_ratio_gauge_takes_a_number_or_off() {
        let set = parse(&valid_toml_with(
            "",
            "compaction_dead_rows_fraction = 0.05\ncompaction_dead_bytes_ratio = 0.0",
        ))
        .unwrap();
        assert_eq!(set.compaction.tombstoned_rows_fraction, Some(0.05));
        assert_eq!(set.compaction.dead_bytes_ratio, Some(0.0));

        let off = parse(&valid_toml_with(
            "",
            "compaction_dead_rows_fraction = \"off\"\ncompaction_dead_bytes_ratio = \"off\"",
        ))
        .unwrap();
        assert_eq!(off.compaction.tombstoned_rows_fraction, None);
        assert_eq!(off.compaction.dead_bytes_ratio, None);

        let err = parse(&valid_toml_with(
            "",
            "compaction_dead_bytes_ratio = \"sometimes\"",
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NotANumberOrOff {
                key: "ingest.compaction_dead_bytes_ratio",
                ..
            }
        ));
    }

    #[test]
    fn the_segment_ceiling_switches_off_on_its_own() {
        let parsed = parse(&valid_toml_with("", "compaction_max_segments = \"off\"")).unwrap();
        assert_eq!(parsed.compaction.max_segments, None);
        assert!(parsed.compaction.window_start_secs.is_some());
    }

    /// The deletion route follows `overlay_soft_limit` unless it is set apart from it.
    #[test]
    fn the_deletion_route_follows_the_overlay_alarm_unless_set_apart_from_it() {
        let followed = parse(&valid_toml_with("", "overlay_soft_limit = 42")).unwrap();
        assert_eq!(followed.compaction.after_deletions, Some(42));

        let apart = parse(&valid_toml_with(
            "",
            "overlay_soft_limit = 42\ncompaction_after_deletions = 9000",
        ))
        .unwrap();
        assert_eq!(apart.compaction.after_deletions, Some(9000));
    }

    #[test]
    fn each_compaction_route_switches_off_independently() {
        let no_window =
            parse(&valid_toml_with("", "compaction_window_start = \"off\"")).unwrap();
        assert_eq!(no_window.compaction.window_start_secs, None);
        assert!(no_window.compaction.after_deletions.is_some());

        let no_depth =
            parse(&valid_toml_with("", "compaction_after_deletions = \"off\"")).unwrap();
        assert_eq!(no_depth.compaction.after_deletions, None);
        assert!(no_depth.compaction.window_start_secs.is_some());

        let err = parse(&valid_toml_with("", "compaction_after_deletions = \"never\""))
            .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::NotANumberOrOff {
                key: "ingest.compaction_after_deletions",
                ..
            }
        ));
    }

    #[test]
    fn the_window_start_is_a_strict_utc_time_of_day() {
        let parsed = parse(&valid_toml_with("", "compaction_window_start = \"02:30\"")).unwrap();
        assert_eq!(parsed.compaction.window_start_secs, Some(2 * 3_600 + 1_800));

        for bad in ["9:30", "24:00", "00:60", "0230", "2:3", "midnight", ""] {
            let toml = valid_toml_with("", &format!("compaction_window_start = \"{bad}\""));
            assert!(
                matches!(parse(&toml), Err(ConfigError::CompactionWindowNotATime(_))),
                "'{bad}' should be refused"
            );
        }
    }

    #[test]
    fn the_build_half_defaults_to_schema_toml_and_the_named_variable() {
        let config = parse(&valid_toml("")).expect("a config naming neither must load");
        assert_eq!(config.schema_path, PathBuf::from(DEFAULT_SCHEMA_FILE));
        assert_eq!(config.identity_env, DEFAULT_IDENTITY_ENV);
    }

    #[test]
    fn the_build_half_is_declarable() {
        let toml = format!(
            "{}\n[build]\nschema = \"corpus/declaration.toml\"\n[identity]\nenv = \"ACME_KEY\"\n",
            valid_toml("")
        );
        let config = parse(&toml).expect("both sections must load");
        assert_eq!(config.schema_path, PathBuf::from("corpus/declaration.toml"));
        assert_eq!(config.identity_env, "ACME_KEY");
    }

    /// The identity key itself is refused in this file, which belongs in git.
    #[test]
    fn a_key_written_into_the_deployment_file_is_refused() {
        let toml = format!("{}\n[identity]\nkey = \"00\"\n", valid_toml(""));
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::IdentityKeyInline)
        ));

        let toml = format!("{}\n[identity]\nenvv = \"X\"\n", valid_toml(""));
        assert!(matches!(
            parse(&toml),
            Err(ConfigError::UnknownIdentityKey(ref k)) if k == "envv"
        ));
    }

    #[test]
    fn the_deployment_file_is_found_by_walking_up_and_its_absence_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();

        assert!(matches!(
            discover(None, &deep),
            Err(ConfigError::NoDeploymentConfig { ref from }) if *from == deep
        ));

        let at = tmp.path().join(DEPLOYMENT_FILE);
        std::fs::write(&at, "").unwrap();
        assert_eq!(discover(None, &deep).unwrap(), at);

        let named = PathBuf::from("/elsewhere/tessera.toml");
        assert_eq!(discover(Some(&named), &deep).unwrap(), named);
    }

    #[test]
    fn paths_resolve_against_the_deployment_files_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let at = tmp.path().join(DEPLOYMENT_FILE);
        std::fs::write(&at, valid_toml("")).unwrap();
        let config = load(&at).expect("the file loads");
        assert_eq!(config.bundle_path, tmp.path().join("b"));
        assert_eq!(config.cache_dir, tmp.path().join("c"));
        assert_eq!(config.wal_path, tmp.path().join("w"));
        assert_eq!(config.schema_path, tmp.path().join(DEFAULT_SCHEMA_FILE));
    }

    /// A credential file resolves against the deployment file's directory, and one that is named
    /// and absent is refused carrying the resolved path.
    #[test]
    fn a_credential_file_resolves_against_the_deployment_files_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let at = tmp.path().join(DEPLOYMENT_FILE);
        std::fs::write(tmp.path().join("session.cred"), "s3cret\n").unwrap();
        std::fs::write(
            &at,
            valid_toml("session_credential_file = \"session.cred\"\n").replace(
                "session_credential_env = \"TESSERA_TEST_SESSION_CRED\"\n",
                "",
            ),
        )
        .unwrap();
        assert!(
            !Path::new("session.cred").exists(),
            "the cwd must not hold one"
        );

        let config = load(&at).expect("the file loads");
        assert_eq!(
            config.session_credential.file.as_deref(),
            Some(tmp.path().join("session.cred").as_path())
        );
        assert_eq!(
            config.session_credential.resolve("session").unwrap(),
            "s3cret"
        );

        let at = tmp.path().join("absent").join(DEPLOYMENT_FILE);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(
            &at,
            valid_toml("operator_credential_file = \"operator.cred\"\n").replace(
                "operator_credential_env = \"TESSERA_TEST_OPERATOR_CRED\"\n",
                "",
            ),
        )
        .unwrap();
        let config = load(&at).expect("the file loads");
        let resolved = tmp.path().join("absent").join("operator.cred");
        assert!(matches!(
            config.operator_credential.resolve("operator"),
            Err(ConfigError::CredentialFileUnreadable { which: "operator", ref path, .. })
                if *path == resolved
        ));
    }

    /// A serving secret is read at startup, never at parse, so a build needs none.
    #[test]
    fn a_serving_credential_is_read_at_startup_rather_than_at_parse() {
        let toml = valid_toml("").replace(
            "TESSERA_TEST_SESSION_CRED",
            "TESSERA_TEST_CREDENTIAL_THAT_IS_NEVER_SET",
        );
        let config = parse(&toml).expect("an unset credential variable must still parse");
        assert!(matches!(
            config.session_credential.resolve("session"),
            Err(ConfigError::MissingCredential("session"))
        ));

        let toml = valid_toml("").replace(
            "session_credential_env = \"TESSERA_TEST_SESSION_CRED\"\n",
            "",
        );
        let config = parse(&toml).expect("a config declaring no session credential still parses");
        assert!(matches!(
            config.session_credential.resolve("session"),
            Err(ConfigError::MissingCredential("session"))
        ));
    }

    #[test]
    fn the_flush_and_merge_knobs_have_working_defaults() {
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(
            config.ingest_buffer_max_items,
            DEFAULT_INGEST_BUFFER_MAX_ITEMS
        );
        assert_eq!(config.segment_floor_bytes, DEFAULT_SEGMENT_FLOOR_BYTES);
        assert_eq!(config.tier_width, DEFAULT_TIER_WIDTH);
        assert_eq!(config.coalesce_width, DEFAULT_COALESCE_WIDTH);
        assert_eq!(config.max_merged_segment_bytes, None);
    }

    #[test]
    fn the_merge_and_coalesce_knobs_parse_when_set() {
        let config = parse(&valid_toml(
            "tier_width = 2\nsegment_floor_bytes = 1\ncoalesce_width = 3",
        ))
        .expect("explicit merge knobs must load");
        assert_eq!(config.tier_width, 2);
        assert_eq!(config.segment_floor_bytes, 1);
        assert_eq!(config.coalesce_width, 3);
    }

    #[test]
    fn the_selection_clauses_have_working_defaults() {
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.k_min, DEFAULT_K_MIN);
        assert_eq!(config.k_max_marks, DEFAULT_K_MAX_MARKS);
        assert_eq!(config.theta_target_marks, DEFAULT_THETA_TARGET_MARKS);
        assert_eq!(config.max_underlay_offset, DEFAULT_MAX_UNDERLAY_OFFSET);
        assert_eq!(config.max_underlay_cells, DEFAULT_MAX_UNDERLAY_CELLS);
        assert_eq!(config.max_k, DEFAULT_MAX_K);
        assert_eq!(
            config.compute_threads,
            tessera_engine::default_compute_threads()
        );
        assert_eq!(
            config.compute_admission,
            COMPUTE_ADMISSION_MULTIPLIER * config.compute_threads
        );
        assert_eq!(config.compute_queue, 2 * config.compute_admission);
        assert_eq!(config.admission_timeout_ms, DEFAULT_ADMISSION_TIMEOUT_MS);
    }

    /// An explicit `compute_threads` moves the default admission bound and queue with it.
    #[test]
    fn compute_admission_defaults_to_compute_threads() {
        let config = parse(&valid_toml("compute_threads = 7")).expect("must load");
        assert_eq!(config.compute_threads, 7);
        assert_eq!(config.compute_admission, 28);
        assert_eq!(config.compute_queue, 56);
    }

    #[test]
    fn a_zero_compute_queue_is_legal() {
        let config = parse(&valid_toml("compute_queue = 0")).expect("compute_queue = 0 must load");
        assert_eq!(config.compute_queue, 0);
    }

    /// An admission gate past `Semaphore::MAX_PERMITS` is refused, whether it was written or
    /// derived from `compute_threads`.
    #[test]
    fn an_admission_gate_past_the_semaphore_limit_refuses_to_start() {
        for (serve, ingest) in [
            (
                "compute_admission = 2000000000000000000\ncompute_queue = 2000000000000000000",
                "",
            ),
            ("compute_threads = 9223372036854775807", ""),
            ("", "ingest_admission = 9223372036854775807"),
        ] {
            let err = parse(&valid_toml_with(serve, ingest)).unwrap_err();
            assert!(
                matches!(err, ConfigError::AdmissionTooLarge { .. }),
                "{serve} {ingest}: {err}"
            );
        }
    }

    #[test]
    fn dev_cors_origins_defaults_to_empty() {
        let config = parse(&valid_toml("")).expect("a config naming no CORS origins must load");
        assert!(config.dev_cors_origins.is_empty());
    }

    #[test]
    fn dev_cors_origins_round_trips_when_named() {
        let toml = valid_toml("dev_cors_origins = [\"http://localhost:5173\"]");
        let config = parse(&toml).expect("a config naming a CORS origin must load");
        assert_eq!(config.dev_cors_origins, vec!["http://localhost:5173"]);
        assert!(config.cors_origins.is_empty());
    }

    #[test]
    fn cors_loopback_defaults_to_false_and_round_trips() {
        let config = parse(&valid_toml("")).expect("a config naming no CORS keys must load");
        assert!(!config.cors_loopback);

        let config = parse(&valid_toml("cors_loopback = true")).expect("the key must load");
        assert!(config.cors_loopback);
        assert!(config.cors_origins.is_empty() && config.dev_cors_origins.is_empty());
    }

    #[test]
    fn cors_origins_defaults_to_empty_and_round_trips_beside_the_dev_key() {
        let config = parse(&valid_toml("")).expect("a config naming no CORS origins must load");
        assert!(config.cors_origins.is_empty());

        let toml = valid_toml(
            "dev_cors_origins = [\"http://localhost:5173\"]\n\
             cors_origins = [\"https://app.example\", \"https://docs.example\"]",
        );
        let config = parse(&toml).expect("both lists together must load");
        assert_eq!(config.dev_cors_origins, vec!["http://localhost:5173"]);
        assert_eq!(
            config.cors_origins,
            vec!["https://app.example", "https://docs.example"]
        );
    }

    #[test]
    fn an_origin_in_both_lists_is_not_an_error() {
        let toml = valid_toml(
            "dev_cors_origins = [\"https://app.example\"]\n\
             cors_origins = [\"https://app.example\"]",
        );
        let config = parse(&toml).expect("a duplicated origin must load");
        assert_eq!(config.dev_cors_origins, config.cors_origins);
    }

    /// A wildcard is refused in either list, whitespace and all, naming the key.
    #[test]
    fn a_wildcard_origin_is_refused_in_either_list() {
        let err = parse(&valid_toml("cors_origins = [\"*\"]")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::CorsWildcard {
                    key: "cors_origins"
                }
            ),
            "{err}"
        );
        let err = parse(&valid_toml(
            "dev_cors_origins = [\"http://localhost:5173\", \" * \"]",
        ))
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::CorsWildcard {
                    key: "dev_cors_origins"
                }
            ),
            "{err}"
        );
    }

    /// A deployment that writes no `[ingest]` section gets every write-path default.
    #[test]
    fn every_stage_2_1_knob_defaults() {
        let toml = valid_toml("");
        assert!(!toml.contains("[ingest]"));
        let config = parse(&toml).expect("a config naming none of the write-path knobs must load");

        assert_eq!(
            config.commit_window_max_items,
            DEFAULT_COMMIT_WINDOW_MAX_ITEMS
        );
        assert_eq!(config.ingest_queue_bound, DEFAULT_INGEST_QUEUE_BOUND);
        assert_eq!(config.ingest_admission, DEFAULT_INGEST_ADMISSION);
        assert_eq!(config.ingest_max_batch_rows, DEFAULT_INGEST_MAX_BATCH_ROWS);
        assert_eq!(
            config.ingest_max_batch_bytes,
            DEFAULT_INGEST_MAX_BATCH_BYTES
        );
        assert_eq!(
            config.publish_max_body_bytes,
            DEFAULT_PUBLISH_MAX_BODY_BYTES
        );
        assert_eq!(
            config.max_artifacts_per_request,
            DEFAULT_MAX_ARTIFACTS_PER_REQUEST
        );
        assert_eq!(
            config.max_members_per_request,
            DEFAULT_MAX_MEMBERS_PER_REQUEST
        );
        assert_eq!(
            config.max_excluded_per_request,
            DEFAULT_MAX_EXCLUDED_PER_REQUEST
        );
        assert_eq!(config.overlay_soft_limit, DEFAULT_OVERLAY_SOFT_LIMIT);
        assert_eq!(config.flush_max_age_secs, DEFAULT_FLUSH_MAX_AGE_SECS);
        assert_eq!(config.flush_max_items, DEFAULT_FLUSH_MAX_ITEMS);
        assert_eq!(
            config.row_projection_cache_bytes,
            DEFAULT_ROW_PROJECTION_CACHE_BYTES
        );
        assert_eq!(config.fragment_cache_bytes, DEFAULT_FRAGMENT_CACHE_BYTES);
    }

    /// The keys are read from their own sections, including a zero, which is the user's to write.
    #[test]
    fn the_stage_2_1_knobs_are_read_from_their_sections() {
        let config = parse(&valid_toml_with(
            "row_projection_cache_bytes = 777000000\nk_min = 0",
            "commit_window_max_items = 7\ningest_queue_bound = 0\nflush_max_items = 55",
        ))
        .expect("must load");
        assert_eq!(config.row_projection_cache_bytes, 777_000_000);
        assert_eq!(config.k_min, 0);
        assert_eq!(config.commit_window_max_items, 7);
        assert_eq!(config.ingest_queue_bound, 0);
        assert_eq!(config.flush_max_items, 55);
    }

    /// A misspelt key, a key in the wrong section, and a misspelt section are all refused.
    #[test]
    fn a_misspelt_key_or_section_refuses_to_start() {
        for toml in [
            valid_toml_with("", "flush_max_age = 9"),
            valid_toml_with("", "row_projection_cache_bytes = 42"),
            valid_toml("commit_window_max_items = 7"),
            valid_toml("").replace("[serve]", "[serv]\n[serve]"),
        ] {
            let err = parse(&toml).unwrap_err();
            assert!(
                matches!(err, ConfigError::Toml(_)),
                "an unrecognised key or section must be refused, got {err}"
            );
        }
    }

    /// The queue-full 429 is reachable at the defaults only while more handlers are admitted than
    /// the queue holds, since each admitted handler holds at most one entry.
    #[test]
    fn the_default_admission_bound_exceeds_the_default_queue_bound() {
        assert!(DEFAULT_INGEST_ADMISSION.saturating_sub(1) > DEFAULT_INGEST_QUEUE_BOUND);
    }

    /// The pool is derived from its consumers, so it covers both admission bounds.
    #[test]
    fn the_serving_blocking_pool_covers_its_declared_consumers() {
        let config = parse(&valid_toml_with(
            "compute_admission = 512",
            "ingest_admission = 64",
        ))
        .expect("a large machine must start");
        let pool = serving_blocking_threads(&config);
        assert!(pool >= config.compute_admission + config.ingest_admission);
        assert!(pool > 512);
    }
}
