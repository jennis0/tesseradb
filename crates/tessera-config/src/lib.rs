//! `tessera.toml`: what a deployment is, read by both `tessera build` and `tessera serve`.
//!
//! Serving secrets are only located here and are read at startup ([`Credential::resolve`]), so a
//! build never needs them. Every raw section refuses a key it does not know.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::sync::Semaphore;

pub mod defaults;
mod error;
#[cfg(test)]
mod reference;

use defaults::*;
pub use error::ConfigError;

pub type Result<T> = std::result::Result<T, ConfigError>;

/// `tessera.toml` describes one deployment: where its bundle and working files are, which corpus
/// declaration `tessera build` reads, and how `tessera serve` listens and bounds its work.
/// `tessera build`, `tessera check`, `tessera health` and `tessera serve` read it from the working
/// directory, or from the nearest directory above it that has one. `--deployment` names another
/// file.
///
/// Every table refuses a key it does not know. A relative path is read from the directory this
/// file is in, except the `unix:` socket path of `[serve] control`. No integer may be negative.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    /// Where the bundle and the server's own files are.
    bundle: RawBundle,
    /// What `tessera build` and `tessera check` read.
    #[serde(default)]
    build: RawBuild,
    /// The authorisation plugin, which turns an access label into the terms a viewer's token is
    /// checked against.
    plugin: RawPlugin,
    /// How long a viewer's token lasts.
    ///
    /// Required.
    disclosure: Option<RawDisclosure>,
    /// How `tessera serve` listens, whom it admits, and the limits on each request. `tessera
    /// build` reads none of it, and a file for building alone may leave the table out.
    #[serde(default)]
    serve: RawServe,
    /// Writes through the control plane: the limits on each request, and when buffered writes
    /// are published and compacted.
    #[serde(default)]
    ingest: RawIngest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundle {
    /// The bundle directory, which `tessera build` writes unless `--out` names another, and
    /// `tessera serve` opens.
    path: PathBuf,
    /// A directory outside the bundle for files the server derives: cached visibility masks,
    /// which survive a restart, and suggestion indexes and scratch files, which are rebuilt at
    /// each start.
    cache: PathBuf,
    /// The write-ahead log. Every write through the control plane is appended and synced to disc
    /// here before it is acknowledged, and the log is replayed when the server starts. The path
    /// names a series of files: `wal.log` is written as `wal-000001.log`, `wal-000002.log` and so
    /// on, and is never itself a file.
    wal: PathBuf,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBuild {
    /// The corpus declaration. `--config` names another.
    ///
    /// Default: `"schema.toml"`.
    schema: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    /// The plugin. This build has one, `builtin:passthrough`, which makes each access label its
    /// own term, and refuses any other name.
    module: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisclosure {
    /// The lifetime, in seconds, of every viewer token `POST /session/authorise` issues. A token
    /// past it is refused with 403. With `0`, a token has expired when it is issued.
    ///
    /// Required.
    token_max_lifetime: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    /// The viewer plane's address and port, such as `"127.0.0.1:8141"`: `/v1/meta`,
    /// `/v1/viewport`, `/v1/items`, `/v1/artifacts` and `/v1/categories`, for requests carrying a
    /// viewer token. `tessera serve` refuses to start without it, and `tessera health` probes it.
    /// Port 0 takes a free port, which the server prints when it starts.
    ///
    /// Default: not set.
    viewer: Option<String>,
    /// The session plane's address and port: `POST /session/authorise`, which issues viewer
    /// tokens, and `POST /session/revoke`. `tessera serve` refuses to start without it.
    ///
    /// Default: not set.
    session: Option<String>,
    /// The control plane's address and port, or `"unix:<path>"` for a Unix socket: ingest,
    /// deletion and suppression, declarations, flush, compaction and status, all under
    /// `/control`. `tessera serve` refuses to start without it. A relative socket path is read
    /// from the server's working directory, not from this file's directory, and a file already
    /// at the path is removed.
    ///
    /// Default: not set.
    control: Option<String>,
    /// A file holding the session credential, the bearer token the session plane requires. Its
    /// contents are trimmed. `tessera serve` refuses to start when the file cannot be read, and
    /// when neither this nor `session_credential_env` is set. When both are set, the file is
    /// used.
    ///
    /// Default: not set.
    session_credential_file: Option<PathBuf>,
    /// An environment variable holding the session credential. `tessera serve` refuses to start
    /// when it is unset.
    ///
    /// Default: not set.
    session_credential_env: Option<String>,
    /// A file holding the operator credential, the bearer token every request to the control
    /// plane requires. Its contents are trimmed. `tessera serve` refuses to start when the file
    /// cannot be read, and when neither this nor `operator_credential_env` is set. When both are
    /// set, the file is used.
    ///
    /// Default: not set.
    operator_credential_file: Option<PathBuf>,
    /// An environment variable holding the operator credential. `tessera serve` refuses to start
    /// when it is unset.
    ///
    /// Default: not set.
    operator_credential_env: Option<String>,
    /// Browser origins, such as `"https://maps.example.org"`, whose pages may call the viewer
    /// plane with a viewer token. `"*"` is refused.
    ///
    /// Default: `[]`.
    cors_origins: Option<Vec<String>>,
    /// Admit a page served from `localhost`, `127.0.0.1` or `[::1]`, on any port, to the viewer
    /// plane, as for a notebook whose port is not known in advance.
    ///
    /// Default: `false`.
    cors_loopback: Option<bool>,
    /// Browser origins whose pages may call both the viewer plane and the session plane, so a
    /// page in development can hold the session credential. The server logs a warning at start
    /// when it is set. `"*"` is refused.
    ///
    /// Default: `[]`.
    dev_cors_origins: Option<Vec<String>>,
    /// Threads in the pool that computes responses.
    ///
    /// Default: the number of CPUs the process may use.
    compute_threads: Option<usize>,
    /// Viewer and session requests computed at once. It admits `/v1/viewport`, the single-item and
    /// single-artifact reads, `/v1/artifacts/browse`, `/v1/aggregate`, a
    /// `/v1/categories/{column}/suggest` with `view` and `counts=true`, and `/session/authorise`,
    /// never the control plane.
    ///
    /// Default: four per compute thread.
    compute_admission: Option<usize>,
    /// Requests that may wait for an admission slot beyond those running. A request finding no
    /// place is refused with 429 at once. `compute_admission` and `compute_queue` together may
    /// not exceed 2305843009213693951.
    ///
    /// Default: twice `compute_admission`.
    compute_queue: Option<usize>,
    /// Milliseconds a queued request waits for an admission slot before it is refused with 429.
    ///
    /// Default: `250`.
    admission_timeout_ms: Option<u64>,
    /// Milliseconds a request waits for another request's build of a shared cached structure
    /// before it is refused with 429.
    ///
    /// Default: `6000`.
    single_flight_wait_ms: Option<u64>,
    /// The largest `k` a viewport request may name; a larger one is lowered to it. A tile draws at
    /// most the smaller of this and `k_max_marks`.
    ///
    /// Default: `1000`.
    max_k: Option<usize>,
    /// The fewest marks a tile with a visible point draws. `tessera serve` refuses to start with
    /// `0`.
    ///
    /// Default: `2`.
    k_min: Option<usize>,
    /// The most marks a tile draws, and the `k` of a request that names none.
    ///
    /// Default: `500`.
    k_max_marks: Option<usize>,
    /// The marks the average occupied tile draws at any zoom; the threshold that samples points
    /// is derived from it.
    ///
    /// Default: `16`.
    theta_target_marks: Option<u64>,
    /// The most tiles one `POST /v1/viewport` may cover. A request covering more is refused with
    /// 422.
    ///
    /// Default: `262144`.
    max_tiles_per_request: Option<usize>,
    /// The largest `underlay_offset` a viewport request may name: how many zoom levels below the
    /// tiles its exact masked counts are served at. A larger one is refused with 422, and `0`
    /// refuses every underlay. At most 255.
    ///
    /// Default: `4`.
    max_underlay_offset: Option<u8>,
    /// The most underlay cells one viewport request may ask for, its tiles times 4 to the power
    /// of its `underlay_offset`. A request asking for more is refused with 422.
    ///
    /// Default: `8192`.
    max_underlay_cells: Option<usize>,
    /// Bytes a streamed viewport response gathers before it sends a frame. A frame always ends
    /// at a whole tile.
    ///
    /// Default: `1048576` (1 MiB).
    stream_flush_bytes: Option<usize>,
    /// Milliseconds a streamed response waits for a client that has stopped reading before it
    /// cuts the response off.
    ///
    /// Default: `10000`.
    stream_write_stall_ms: Option<u64>,
    /// Milliseconds a streamed response may run. A viewport response is cut off at it. A bulk
    /// read ends at it with a cursor to resume from.
    ///
    /// Default: `60000`.
    stream_deadline_ms: Option<u64>,
    /// Add each stage's timing, as `stage_ns`, to the last frame of a viewport response. It has
    /// an effect only in a binary built with the `bench-timing` feature.
    ///
    /// Default: `false`.
    stage_timing: Option<bool>,
    /// The most vertices a `region` filter's polygon may have. A filter with more is refused
    /// with 422.
    ///
    /// Default: `10000`.
    max_region_vertices: Option<u64>,
    /// The most boundary cells a `region` filter is resolved to at one zoom level. Past it the
    /// filter is answered for a cover of the polygon, which the `x-tessera-region` header
    /// reports, rather than refused.
    ///
    /// Default: `262144`.
    max_region_cells: Option<usize>,
    /// Bytes of resolved `region` filters kept for reuse, shared by every viewer.
    ///
    /// Default: `268435456` (256 MiB).
    region_cache_bytes: Option<u64>,
    /// The most values one page of `GET /v1/categories/{column}` returns, and the page size of a
    /// request that names none. A larger `limit` is lowered to it.
    ///
    /// Default: `1000`.
    max_category_values: Option<usize>,
    /// The most values one `/v1/categories/{column}/suggest` returns, `GET` or `POST`, and the
    /// `limit` of a request that names none.
    ///
    /// Default: `20`.
    max_suggestions: Option<usize>,
    /// The most values one suggestion request examines, hidden ones included, before it stops
    /// and answers `more: true`.
    ///
    /// Default: `100000`.
    max_suggestion_walk: Option<u64>,
    /// The size of a viewer's visible set at or below which suggestions are answered from a set
    /// of the values that viewer can see, built once per session, rather than by checking each
    /// value in turn.
    ///
    /// Default: `10000000`.
    max_suggest_set_entities: Option<u64>,
    /// The most rows one page of `POST /v1/artifacts/browse` returns, and the page size of a
    /// request that names none.
    ///
    /// Default: `200`.
    max_browse_rows: Option<usize>,
    /// The most vertices a shape published through `/control/layers/{name}/artifacts` may have.
    /// A shape with more is refused with 422. A build does not read this key: it refuses a shape
    /// of more than 1000000 vertices whatever the key says.
    ///
    /// Default: `1000000`.
    max_shape_vertices: Option<u64>,
    /// The most rows one page of a bulk read, `POST /v1/items` or `POST /v1/artifacts`, holds.
    /// `0` is refused.
    ///
    /// Default: `100000`.
    max_page_rows: Option<u32>,
    /// The most bytes one page of a bulk read holds, as Arrow before compression. A row larger
    /// than this is sent alone. `0` is refused, and so is a value above 2147483648.
    ///
    /// Default: `67108864` (64 MiB).
    max_page_bytes: Option<usize>,
    /// Bulk reads running at once. One more is refused with 429 at once, and `0` refuses every
    /// bulk read. Bulk reads may hold seven times `max_page_bytes` of memory each. A value above
    /// 2305843009213693951 is refused.
    ///
    /// Default: `2`.
    bulk_admission: Option<usize>,
    /// The most bytes one bulk-read response carries before it ends with a cursor to resume from.
    /// A value below `max_page_bytes` is refused.
    ///
    /// Default: `268435456` (256 MiB).
    bulk_response_bytes: Option<usize>,
    /// Milliseconds one bulk-read response may run before it ends with a cursor to resume from.
    /// `stream_deadline_ms` ends one too, whichever comes first.
    ///
    /// Default: `30000`.
    bulk_response_ms: Option<u64>,
    /// The most groupings one `POST /v1/aggregate` may ask for, each a table of the response. A
    /// request with more is refused with 422.
    ///
    /// Default: `16`.
    max_aggregate_groupings: Option<u32>,
    /// The largest `top` one aggregate grouping may ask for. A larger one is refused with 422.
    ///
    /// Default: `1000`.
    max_aggregate_top: Option<u32>,
    /// The most values or artifacts one aggregate grouping may name. A longer list is refused
    /// with 422.
    ///
    /// Default: `1000`.
    max_aggregate_named: Option<u32>,
    /// The most cells one aggregate grouping's cell level may list: the cells at its depth in its
    /// area, however many groups share them. A request asking for more is refused with 422.
    ///
    /// Default: `1048576`.
    max_aggregate_cells: Option<u64>,
    /// The most bytes one `POST /v1/aggregate` response carries before it ends with a cursor to
    /// resume from. It runs under the viewport's admission, so this bounds what each one holds.
    /// A value below `aggregate_page_bytes` or above 2147483648 is refused.
    ///
    /// Default: `16777216` (16 MiB).
    aggregate_response_bytes: Option<usize>,
    /// The most bytes one page of an aggregate response holds, as Arrow column bytes before
    /// compression. `0` is refused, and so is a value above 2147483648.
    ///
    /// Default: `4194304` (4 MiB).
    aggregate_page_bytes: Option<usize>,
    /// The most seconds a write asking to wait until it is visible, and `/control/flush`, wait
    /// before answering `visible: false`.
    ///
    /// Default: `30`.
    visible_wait_max_secs: Option<u64>,
    /// Bytes of each viewer's projection of the rows they may see, kept between requests.
    ///
    /// Default: `2147483648` (2 GiB).
    row_projection_cache_bytes: Option<u64>,
    /// Bytes of cached visibility masks kept in memory. Masks also persist in the `[bundle]`
    /// cache directory, which this does not bound.
    ///
    /// Default: `1073741824` (1 GiB).
    fragment_cache_bytes: Option<u64>,
    /// Bytes of per-viewer artifact counts kept for layers served from one bitmap per artifact.
    ///
    /// Default: `268435456` (256 MiB).
    masked_count_cache_bytes: Option<u64>,
    /// Bytes of per-viewer counts of occupied tiles, from which the sampling threshold is derived.
    ///
    /// Default: `33554432` (32 MiB).
    occupancy_cache_bytes: Option<u64>,
    /// Segments at or below this size are treated as one size tier when choosing which to merge.
    ///
    /// Default: `16777216` (16 MiB).
    segment_floor_bytes: Option<u64>,
    /// How many segments of one size tier are merged together. `tessera serve` refuses to start
    /// with a value below 2.
    ///
    /// Default: `4`.
    tier_width: Option<usize>,
    /// The largest segment a merge may produce, in bytes.
    ///
    /// Default: not set, and the server merges up to 268435456 bytes (256 MiB).
    max_merged_segment_bytes: Option<u64>,
    /// How many small files of one size tier, which flushes write for attribute values, records,
    /// text indexes and access terms, are combined into one. Unique index runs are combined four
    /// at a time whatever this says. `tessera serve` refuses to start with a value below 2.
    ///
    /// Default: `8`.
    coalesce_width: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIngest {
    /// Requests to `/control/ingest` handled at once. One more is refused with 429 at once. A value
    /// above 2305843009213693951 is refused.
    ///
    /// Default: `64`.
    ingest_admission: Option<usize>,
    /// Ingest batches that may wait for the writer. When the queue is full an ingest is refused
    /// with 429 and a `Retry-After`. Deletions and suppressions do not wait in it and are never
    /// refused for load.
    ///
    /// Default: `32`.
    ingest_queue_bound: Option<usize>,
    /// Ingested items that may wait for a flush. At or above it `/control/ingest` is refused
    /// with 429 before anything is written.
    ///
    /// Default: `1000000`.
    ingest_buffer_max_items: Option<usize>,
    /// The most rows one `/control/ingest` request may carry. A request with more is refused with
    /// 422.
    ///
    /// Default: `10000`.
    ingest_max_batch_rows: Option<usize>,
    /// The largest body `/control/ingest` accepts, in bytes. A larger one is refused with 422.
    ///
    /// Default: `16777216` (16 MiB).
    ingest_max_batch_bytes: Option<usize>,
    /// Rows at which a commit window closes. Items allocated ids in one window are grouped by the
    /// access terms they carry, so a wider window stores the access index more compactly. `0` is
    /// read as 1.
    ///
    /// Default: `10000`.
    commit_window_max_items: Option<usize>,
    /// Seconds between flushes, which publish buffered writes as a new segment and make them
    /// visible.
    ///
    /// Default: `90`.
    flush_max_age_secs: Option<u64>,
    /// Buffered rows at which a flush runs before `flush_max_age_secs` has passed.
    ///
    /// Default: `40000`.
    flush_max_items: Option<usize>,
    /// The largest body `PUT` and `PATCH /control/layers/{name}/artifacts` accept, in bytes. A
    /// larger one is refused with 422.
    ///
    /// Default: `67108864` (64 MiB).
    publish_max_body_bytes: Option<usize>,
    /// The most artifacts one `PUT /control/layers/{name}/artifacts` may publish. More is
    /// refused with 422.
    ///
    /// Default: `10000`.
    max_artifacts_per_request: Option<usize>,
    /// The most members one `PATCH /control/layers/{name}/artifacts` may add and remove,
    /// counted together. More is refused with 422.
    ///
    /// Default: `5000000`.
    max_members_per_request: Option<usize>,
    /// The most items one published artifact's `excluding` list may name. More is refused with
    /// 422.
    ///
    /// Default: `1000000`.
    max_excluded_per_request: Option<usize>,
    /// Deletions and suppressions held in memory at which the server logs a warning and counts
    /// an alarm. Nothing is refused.
    ///
    /// Default: `500000`.
    overlay_soft_limit: Option<usize>,
    /// Seconds after one compaction starts during which the schedule starts no other.
    ///
    /// Default: `86400`.
    compaction_min_interval_secs: Option<u64>,
    /// When the daily compaction window opens, as `"HH:MM"` in UTC, or `"off"` for no window.
    /// Any other value is refused.
    ///
    /// Default: `"00:00"`.
    compaction_window_start: Option<String>,
    /// How long the window stays open, in seconds. `0` never opens it, and a day or more never
    /// closes it.
    ///
    /// Default: `14400`.
    compaction_window_secs: Option<u32>,
    /// Inside the window, compact when a view has at least this many segments. `0` never
    /// compacts in the window.
    ///
    /// Default: `8`.
    compaction_window_min_segments: Option<usize>,
    /// At any hour, compact when a view has this many segments, or `"off"`.
    ///
    /// Type: integer or `"off"`.
    ///
    /// Default: `64`.
    compaction_max_segments: Option<OrOff<u64>>,
    /// At any hour, compact when this many deleted items wait to be removed, or `"off"`.
    ///
    /// Type: integer or `"off"`.
    ///
    /// Default: the value of `overlay_soft_limit`.
    compaction_after_deletions: Option<OrOff<u64>>,
    /// At any hour, compact when deleted items waiting to be removed reach this fraction of the
    /// stored rows, or `"off"`.
    ///
    /// Type: number or `"off"`.
    ///
    /// Default: `0.2`.
    compaction_dead_rows_fraction: Option<OrOff<f64>>,
    /// At any hour, compact when the bundle's unreferenced bytes on disc reach this ratio of the
    /// bytes its manifests name, or `"off"`.
    ///
    /// Type: number or `"off"`.
    ///
    /// Default: `1.0`.
    compaction_dead_bytes_ratio: Option<OrOff<f64>>,
}

/// A number, or the word `"off"`. Any other word is refused by [`or_off`].
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum OrOff<T> {
    Value(T),
    Word(String),
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
    /// A bulk read's rows per page.
    pub max_page_rows: u32,
    /// A bulk read's Arrow bytes per page, before compression.
    pub max_page_bytes: usize,
    /// Bulk reads running at once. One past it is refused with a 429 at once, never queued, and
    /// `0` refuses every bulk read.
    pub bulk_admission: usize,
    /// Bytes one bulk-read response may carry; at least `max_page_bytes`.
    pub bulk_response_bytes: usize,
    /// Time one bulk-read response may run. `stream_deadline_ms` also ends one, whichever comes
    /// first, and either way the response ends with a cursor to resume from.
    pub bulk_response_ms: u64,
    /// Groupings per `POST /v1/aggregate`.
    pub max_aggregate_groupings: u32,
    /// The largest `top` of an aggregate grouping.
    pub max_aggregate_top: u32,
    /// The longest named list of an aggregate grouping.
    pub max_aggregate_named: u32,
    /// The most cells an aggregate grouping's cell level may list.
    pub max_aggregate_cells: u64,
    /// Bytes one aggregate response may carry; at least `aggregate_page_bytes`.
    pub aggregate_response_bytes: usize,
    /// An aggregate page's Arrow bytes, before compression.
    pub aggregate_page_bytes: usize,
    pub region_cache_bytes: u64,
    /// Add `stage_ns` to a viewport response's trailer; does nothing in a binary built without
    /// `bench-timing`.
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
    /// The most entities one publication's `excluding` list may name.
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

/// The serving runtime's `max_blocking_threads`: one per request any admission bound lets
/// through, plus [`BLOCKING_THREAD_RESERVE`]. A queued request holds none.
pub fn serving_blocking_threads(config: &Config) -> usize {
    config
        .compute_admission
        .saturating_add(config.bulk_admission)
        .saturating_add(config.ingest_admission)
        .saturating_add(BLOCKING_THREAD_RESERVE)
}

/// The memory bulk reads may hold at once: [`BULK_READ_PAGES_HELD`] pages of
/// `max_page_bytes` for each read it admits.
pub fn bulk_read_memory_bytes(config: &Config) -> usize {
    config
        .bulk_admission
        .saturating_mul(BULK_READ_PAGES_HELD)
        .saturating_mul(config.max_page_bytes)
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

pub const DEPLOYMENT_FILE: &str = "tessera.toml";

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
    let bulk_admission = serve.bulk_admission.unwrap_or(DEFAULT_BULK_ADMISSION);
    if bulk_admission > Semaphore::MAX_PERMITS {
        return Err(ConfigError::AdmissionTooLarge {
            key: "serve.bulk_admission",
        });
    }
    let max_page_rows = serve.max_page_rows.unwrap_or(DEFAULT_MAX_PAGE_ROWS);
    if max_page_rows == 0 {
        return Err(ConfigError::Zero {
            key: "serve.max_page_rows",
        });
    }
    let max_page_bytes = serve.max_page_bytes.unwrap_or(DEFAULT_MAX_PAGE_BYTES);
    if max_page_bytes == 0 {
        return Err(ConfigError::Zero {
            key: "serve.max_page_bytes",
        });
    }
    if max_page_bytes > MAX_PAGE_BYTES_CEILING {
        return Err(ConfigError::PageBytesTooLarge {
            value: max_page_bytes,
        });
    }
    let bulk_response_bytes = serve
        .bulk_response_bytes
        .unwrap_or(DEFAULT_BULK_RESPONSE_BYTES);
    if bulk_response_bytes < max_page_bytes {
        return Err(ConfigError::ResponseBelowPage {
            response_bytes: bulk_response_bytes,
            page_bytes: max_page_bytes,
        });
    }
    let stream_deadline_ms = serve
        .stream_deadline_ms
        .unwrap_or(DEFAULT_STREAM_DEADLINE_MS);
    let bulk_response_ms = serve.bulk_response_ms.unwrap_or(DEFAULT_BULK_RESPONSE_MS);
    let aggregate_page_bytes = serve
        .aggregate_page_bytes
        .unwrap_or(DEFAULT_AGGREGATE_PAGE_BYTES);
    if aggregate_page_bytes == 0 {
        return Err(ConfigError::Zero {
            key: "serve.aggregate_page_bytes",
        });
    }
    let aggregate_response_bytes = serve
        .aggregate_response_bytes
        .unwrap_or(DEFAULT_AGGREGATE_RESPONSE_BYTES);
    for (key, value) in [
        ("serve.aggregate_page_bytes", aggregate_page_bytes),
        ("serve.aggregate_response_bytes", aggregate_response_bytes),
    ] {
        if value > MAX_PAGE_BYTES_CEILING {
            return Err(ConfigError::AggregateBytesTooLarge { key, value });
        }
    }
    if aggregate_response_bytes < aggregate_page_bytes {
        return Err(ConfigError::AggregateResponseBelowPage {
            response_bytes: aggregate_response_bytes,
            page_bytes: aggregate_page_bytes,
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
        max_page_rows,
        max_page_bytes,
        bulk_admission,
        bulk_response_bytes,
        bulk_response_ms,
        max_aggregate_groupings: serve
            .max_aggregate_groupings
            .unwrap_or(DEFAULT_MAX_AGGREGATE_GROUPINGS),
        max_aggregate_top: serve.max_aggregate_top.unwrap_or(DEFAULT_MAX_AGGREGATE_TOP),
        max_aggregate_named: serve
            .max_aggregate_named
            .unwrap_or(DEFAULT_MAX_AGGREGATE_NAMED),
        max_aggregate_cells: serve
            .max_aggregate_cells
            .unwrap_or(DEFAULT_MAX_AGGREGATE_CELLS),
        aggregate_response_bytes,
        aggregate_page_bytes,
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
        stream_deadline_ms,
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

    fn valid_toml(serve_extra: &str) -> String {
        valid_toml_with(serve_extra, "")
    }

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
    fn the_build_half_defaults_to_schema_toml() {
        let config = parse(&valid_toml("")).expect("a config naming no schema must load");
        assert_eq!(config.schema_path, PathBuf::from(DEFAULT_SCHEMA_FILE));
    }

    #[test]
    fn the_build_half_is_declarable() {
        let toml = format!(
            "{}\n[build]\nschema = \"corpus/declaration.toml\"\n",
            valid_toml("")
        );
        let config = parse(&toml).expect("the section must load");
        assert_eq!(config.schema_path, PathBuf::from("corpus/declaration.toml"));
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

    #[test]
    fn the_bulk_read_keys_have_their_defaults_and_parse_when_set() {
        let config = parse(&valid_toml("")).expect("defaults must load");
        assert_eq!(config.max_page_rows, DEFAULT_MAX_PAGE_ROWS);
        assert_eq!(config.max_page_bytes, DEFAULT_MAX_PAGE_BYTES);
        assert_eq!(config.bulk_admission, DEFAULT_BULK_ADMISSION);
        assert_eq!(config.bulk_response_bytes, DEFAULT_BULK_RESPONSE_BYTES);
        assert_eq!(config.bulk_response_ms, DEFAULT_BULK_RESPONSE_MS);
        assert_eq!(bulk_read_memory_bytes(&config), 2 * 7 * 64 * 1024 * 1024);

        let config = parse(&valid_toml(
            "max_page_rows = 10\nmax_page_bytes = 4096\nbulk_admission = 3\n\
             bulk_response_bytes = 4096\nbulk_response_ms = 5\nstream_deadline_ms = 6",
        ))
        .expect("explicit bulk-read keys must load");
        assert_eq!(config.max_page_rows, 10);
        assert_eq!(config.max_page_bytes, 4096);
        assert_eq!(config.bulk_admission, 3);
        assert_eq!(config.bulk_response_bytes, 4096);
        assert_eq!(config.bulk_response_ms, 5);
        assert_eq!(
            serving_blocking_threads(&config),
            config.compute_admission + 3 + config.ingest_admission + BLOCKING_THREAD_RESERVE
        );
    }

    #[test]
    fn the_aggregate_byte_keys_are_refused_where_no_response_could_proceed() {
        assert!(matches!(
            parse(&valid_toml("aggregate_page_bytes = 0")),
            Err(ConfigError::Zero { key: "serve.aggregate_page_bytes" })
        ));
        for keys in [
            "aggregate_page_bytes = 4294967296\naggregate_response_bytes = 4294967296",
            "aggregate_response_bytes = 4294967296",
        ] {
            assert!(
                matches!(parse(&valid_toml(keys)), Err(ConfigError::AggregateBytesTooLarge { .. })),
                "{keys}"
            );
        }
        assert!(matches!(
            parse(&valid_toml("aggregate_page_bytes = 4096\naggregate_response_bytes = 4095")),
            Err(ConfigError::AggregateResponseBelowPage {
                response_bytes: 4095,
                page_bytes: 4096
            })
        ));
    }

    #[test]
    fn the_bulk_read_keys_are_refused_where_no_read_could_proceed() {
        for (keys, key) in [
            ("max_page_rows = 0", "serve.max_page_rows"),
            ("max_page_bytes = 0", "serve.max_page_bytes"),
        ] {
            assert!(
                matches!(parse(&valid_toml(keys)), Err(ConfigError::Zero { key: k }) if k == key),
                "{keys}"
            );
        }
        assert!(matches!(
            parse(&valid_toml("max_page_bytes = 4294967296")),
            Err(ConfigError::PageBytesTooLarge { .. })
        ));
        assert!(matches!(
            parse(&valid_toml("max_page_bytes = 4096\nbulk_response_bytes = 4095")),
            Err(ConfigError::ResponseBelowPage {
                response_bytes: 4095,
                page_bytes: 4096
            })
        ));
        // A response time at or past the stream deadline loads: the deadline ends such a read
        // with a trailer to resume from.
        assert!(parse(&valid_toml("bulk_response_ms = 60000\nstream_deadline_ms = 1")).is_ok());
        assert!(matches!(
            parse(&valid_toml(&format!("bulk_admission = {}", usize::MAX / 2))),
            Err(ConfigError::AdmissionTooLarge {
                key: "serve.bulk_admission"
            })
        ));
    }

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

    #[test]
    fn the_default_admission_bound_exceeds_the_default_queue_bound() {
        assert!(DEFAULT_INGEST_ADMISSION.saturating_sub(1) > DEFAULT_INGEST_QUEUE_BOUND);
    }

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
