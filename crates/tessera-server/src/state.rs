//! Shared server state: the engine, the session registry and the admission bounds. The engine
//! never checks a session's deadline after minting it, so expiry, retention and revocation live
//! here.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use tessera_engine::{Engine, Session};

use crate::error::ApiError;

/// The current Unix second, the wall clock `Session::expires_at` is minted on. Every expiry check
/// reads it here, so no two can disagree about the clock.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

/// The retained count below which no sweep runs, so a small registry is not scanned on every
/// insert. A quiet process keeps up to this many expired sessions until the next authorisation.
const SWEEP_FLOOR_ENTRIES: usize = 16;

/// Who a session or a control-plane request acts for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// The holder of the operator credential. It is not in the catalogue and holds no session.
    Superuser,
    /// A principal stored in the catalogue, by name.
    Local(String),
    /// An OIDC identity, named by its issuer and subject, and the provider that accepted it.
    Oidc {
        provider: String,
        issuer: String,
        subject: String,
    },
}

/// One live session: the engine's session and what authorised it.
pub struct SessionEntry {
    pub session: Arc<Session>,
    pub principal: Principal,
    /// The API key the session was authenticated with at login, or minted with through
    /// `authorise-as`, by prefix.
    pub api_key: Option<String>,
    /// The principal that minted the session through `authorise-as`.
    pub minted_by: Option<String>,
    pub permissions: tessera_catalogue::PermissionSet,
    pub created_at: u64,
    /// The earliest of the configured lifetime, the API key's expiry and the OIDC token's `exp`.
    pub expires_at: u64,
}

impl SessionEntry {
    /// Whether a catalogue change reported in `affected` ends this session.
    fn affected_by(&self, affected: &tessera_catalogue::Affected) -> bool {
        let principal = match &self.principal {
            Principal::Local(name) => affected.principals.contains(name),
            Principal::Oidc { provider, .. } => affected.providers.contains(provider),
            Principal::Superuser => false,
        };
        principal
            || self
                .minted_by
                .as_ref()
                .is_some_and(|p| affected.principals.contains(p))
            || self
                .api_key
                .as_ref()
                .is_some_and(|k| affected.api_keys.contains(k))
    }
}

/// A token's place in the registry. An ended session keeps its token until the next sweep, so its
/// holder is answered `403 expired-token` and authorises again.
enum Slot {
    Live(Arc<SessionEntry>),
    Ended,
}

/// Every session, by bearer token and by `token_id`. Expired and ended sessions are refused at
/// lookup and swept on insert once the registry holds twice its live set, since each live one
/// pins a mapped mask fragment. A swept token answers 401.
pub struct SessionRegistry {
    by_token: FxHashMap<String, Slot>,
    /// Live sessions only.
    token_id_to_token: FxHashMap<u64, String>,
    /// Retained count at which [`Self::insert`] runs a sweep. See [`next_sweep_threshold`].
    sweep_at: usize,
    sweeps: u64,
    swept_total: u64,
}

impl Default for SessionRegistry {
    /// Written out so that `sweep_at` starts at the floor rather than zero.
    fn default() -> Self {
        SessionRegistry {
            by_token: FxHashMap::default(),
            token_id_to_token: FxHashMap::default(),
            sweep_at: SWEEP_FLOOR_ENTRIES,
            sweeps: 0,
            swept_total: 0,
        }
    }
}

/// The retained count at which the next sweep runs: twice the live set, which keeps sweeping
/// amortised O(1) per insert, and never below the floor. Saturating, so a huge count cannot wrap.
fn next_sweep_threshold(live: usize) -> usize {
    live.saturating_mul(2).max(SWEEP_FLOOR_ENTRIES)
}

/// Drops every index entry whose token `live` rejects and returns the dropped ids. Separate so a
/// test can reach it, since only the engine can build a [`Session`].
fn prune_index(index: &mut FxHashMap<u64, String>, live: impl Fn(&str) -> bool) -> Vec<u64> {
    let mut dropped = Vec::new();
    index.retain(|token_id, token| {
        let keep = live(token);
        if !keep {
            dropped.push(*token_id);
        }
        keep
    });
    dropped
}

/// `/control/status`'s `sessions` block.
pub struct SessionRegistryStats {
    /// Sessions retained, live, ended and expired but not yet swept: the size of a sweep's pass.
    pub retained: usize,
    /// Sweeps run since process start.
    pub sweeps: u64,
    /// Entries removed by sweeps in total. `retained` rising while this stays at zero means nothing
    /// is being shed.
    pub swept_total: u64,
    /// The retained count at which the next sweep runs.
    pub sweep_at: usize,
}

impl SessionRegistry {
    /// Inserts a freshly authorised session and sweeps if the registry has reached its threshold.
    /// `now_secs` is the caller's wall-clock reading. Returns the swept token ids, for
    /// [`AppState::register_session`] to prune after dropping this lock, which every viewer
    /// request takes.
    fn insert(&mut self, entry: SessionEntry, now_secs: u64) -> Vec<u64> {
        let token = entry.session.token().to_string();
        let token_id = entry.session.token_id();
        self.by_token
            .insert(token.clone(), Slot::Live(Arc::new(entry)));
        self.token_id_to_token.insert(token_id, token);
        if self.by_token.len() >= self.sweep_at {
            self.sweep_expired(now_secs)
        } else {
            Vec::new()
        }
    }

    /// Drops every ended session and every session whose deadline has passed, with exactly the
    /// refusal's predicate, so a sweep removes only what [`AppState::authenticated_session`]
    /// already refuses. Returns the swept token ids that were still indexed.
    fn sweep_expired(&mut self, now_secs: u64) -> Vec<u64> {
        let before = self.by_token.len();
        self.by_token.retain(|_, slot| match slot {
            Slot::Live(entry) => entry.expires_at > now_secs,
            Slot::Ended => false,
        });
        // Pruned against the primary map so the two cannot disagree.
        let by_token = &self.by_token;
        let swept = prune_index(&mut self.token_id_to_token, |token| {
            by_token.contains_key(token)
        });
        self.sweeps += 1;
        self.swept_total += (before - self.by_token.len()) as u64;
        self.sweep_at = next_sweep_threshold(self.by_token.len());
        debug_assert_eq!(
            self.by_token.len(),
            self.token_id_to_token.len(),
            "after a sweep every retained session is live and indexed"
        );
        swept
    }

    /// The session behind `token`: `Ok(None)` for a token never issued or already swept, and
    /// `Err(())` for one that has ended.
    #[allow(clippy::result_unit_err)]
    pub fn get(&self, token: &str) -> Result<Option<Arc<SessionEntry>>, ()> {
        match self.by_token.get(token) {
            None => Ok(None),
            Some(Slot::Live(entry)) => Ok(Some(Arc::clone(entry))),
            Some(Slot::Ended) => Err(()),
        }
    }

    /// The live session with `token_id`.
    pub fn by_id(&self, token_id: u64) -> Option<Arc<SessionEntry>> {
        let token = self.token_id_to_token.get(&token_id)?;
        match self.by_token.get(token) {
            Some(Slot::Live(entry)) => Some(Arc::clone(entry)),
            _ => None,
        }
    }

    /// Ends every live session `doomed` selects. Each holder is answered `403 expired-token` until
    /// the next sweep. Returns the ended token ids, for [`AppState::end_sessions`] to prune.
    fn end_where(&mut self, doomed: impl Fn(&SessionEntry) -> bool) -> Vec<u64> {
        let mut ended = Vec::new();
        for slot in self.by_token.values_mut() {
            if let Slot::Live(entry) = slot {
                if doomed(entry) {
                    ended.push(entry.session.token_id());
                    *slot = Slot::Ended;
                }
            }
        }
        for token_id in &ended {
            self.token_id_to_token.remove(token_id);
        }
        ended
    }

    /// Every live session `keep` selects, in `token_id` order.
    pub fn list(&self, keep: impl Fn(&SessionEntry) -> bool) -> Vec<Arc<SessionEntry>> {
        let mut out: Vec<Arc<SessionEntry>> = self
            .by_token
            .values()
            .filter_map(|slot| match slot {
                Slot::Live(entry) if keep(entry) => Some(Arc::clone(entry)),
                _ => None,
            })
            .collect();
        out.sort_by_key(|e| e.session.token_id());
        out
    }

    pub fn stats(&self) -> SessionRegistryStats {
        SessionRegistryStats {
            retained: self.by_token.len(),
            sweeps: self.sweeps,
            swept_total: self.swept_total,
            sweep_at: self.sweep_at,
        }
    }
}

/// The slot and compute permits from [`ComputeGate::admit`], moved together into the blocking
/// closure. They are released when the work finishes, so a permit tracks compute, not whether
/// the client is still waiting.
pub struct GatePermits {
    _slot: OwnedSemaphorePermit,
    /// `None` once [`Self::release_compute`] has run, marking the streaming emit phase.
    compute: Option<OwnedSemaphorePermit>,
    /// The gate's [`ComputeGate::streaming`] gauge.
    streaming: Arc<AtomicUsize>,
}

impl GatePermits {
    /// Releases the compute permit when the sweep is done and keeps the slot for the emit phase,
    /// which runs at the client's pace; holding compute for it would let slow readers starve the
    /// gate. Idempotent.
    pub fn release_compute(&mut self) {
        if let Some(permit) = self.compute.take() {
            drop(permit);
            self.streaming.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for GatePermits {
    fn drop(&mut self) {
        // `compute` is `None` exactly when `release_compute` ran.
        if self.compute.is_none() {
            self.streaming.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Admission for the CPU-bound viewer and session routes, never the control plane. `slots`
/// bounds admitted requests and sheds at once when full; `compute` bounds running requests and
/// sheds a caller that waits longer than `admission_timeout_ms`. Bulk reads run under their own
/// admission limit, `serve.bulk_admission` ([`ComputeGate::for_bulk_reads`]), so a long read takes
/// no slot from the viewport and item routes.
pub struct ComputeGate {
    pub compute_admission: usize,
    pub compute_queue: usize,
    pub admission_timeout_ms: u64,
    /// What a shed from this gate says it was.
    cause: crate::error::ShedCause,
    slots: Arc<Semaphore>,
    compute: Arc<Semaphore>,
    /// 429s from this gate's two shed paths, with no per-principal label. Single-flight sheds come
    /// after admission and are not counted, though a request waiting on a build holds its permits.
    shed_total: AtomicU64,
    /// Requests in their emit phase: slot held, compute released. Without it, `waiting` would count
    /// every stream as queued.
    streaming: Arc<AtomicUsize>,
}

/// `/control/status`'s `compute` block, derived from the semaphores at read time.
pub struct ComputeGateStatus {
    pub admission: usize,
    pub queue: usize,
    pub in_flight: usize,
    pub waiting: usize,
    /// Emit-phase streams: slot held, compute released.
    pub streaming: usize,
    /// This gate's sheds only, not single-flight ones.
    pub shed_total: u64,
}

impl ComputeGate {
    pub fn new(compute_admission: usize, compute_queue: usize, admission_timeout_ms: u64) -> Self {
        Self::with_cause(
            compute_admission,
            compute_queue,
            admission_timeout_ms,
            crate::error::ShedCause::ComputeGate,
        )
    }

    /// The limit on password checks: `admission` at once and as many waiting, each for at most
    /// `admission_timeout_ms`. One past that is shed with a 429.
    pub fn for_passwords(admission: usize, admission_timeout_ms: u64) -> Self {
        Self::with_cause(
            admission,
            admission,
            admission_timeout_ms,
            crate::error::ShedCause::PasswordGate,
        )
    }

    /// The limit on bulk reads: `admission` at once and none waiting, so one past it is shed with
    /// a 429 at once. A bulk read never releases its compute permit early, so taking the slot
    /// is taking the permit and nothing waits for one.
    pub fn for_bulk_reads(admission: usize) -> Self {
        Self::with_cause(admission, 0, 0, crate::error::ShedCause::BulkGate)
    }

    fn with_cause(
        compute_admission: usize,
        compute_queue: usize,
        admission_timeout_ms: u64,
        cause: crate::error::ShedCause,
    ) -> Self {
        ComputeGate {
            compute_admission,
            compute_queue,
            admission_timeout_ms,
            cause,
            slots: Arc::new(Semaphore::new(compute_admission + compute_queue)),
            compute: Arc::new(Semaphore::new(compute_admission)),
            shed_total: AtomicU64::new(0),
            streaming: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Takes a slot, then a compute permit. Returns the permits, to move into the blocking closure,
    /// and the wait in microseconds for `x-tessera-admission-us`. Every shed is counted once.
    pub async fn admit(&self) -> Result<(GatePermits, u64), crate::error::ApiError> {
        let start = Instant::now();

        // Stage 1: no free slot sheds at once.
        let slot = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                crate::request_log::note_admission_us(0);
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                return Err(crate::error::ApiError::Backpressure {
                    retry_after_s: crate::error::RETRY_AFTER_SECS,
                    cause: self.cause,
                });
            }
        };

        // Stage 2: wait for compute while holding the slot, which is what bounds the queue at
        // `compute_queue`.
        let compute = match tokio::time::timeout(
            Duration::from_millis(self.admission_timeout_ms),
            Arc::clone(&self.compute).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            // The semaphore is never closed; a closed one is treated as a timeout.
            Ok(Err(_)) | Err(_) => {
                crate::request_log::note_admission_us(start.elapsed().as_micros() as u64);
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                return Err(crate::error::ApiError::Backpressure {
                    retry_after_s: crate::error::RETRY_AFTER_SECS,
                    cause: self.cause,
                });
            }
        };

        let admission_us = start.elapsed().as_micros() as u64;
        crate::request_log::note_admission_us(admission_us);
        Ok((
            GatePermits {
                _slot: slot,
                compute: Some(compute),
                streaming: Arc::clone(&self.streaming),
            },
            admission_us,
        ))
    }

    pub fn status(&self) -> ComputeGateStatus {
        let in_flight = self.compute_admission - self.compute.available_permits();
        let admitted =
            (self.compute_admission + self.compute_queue) - self.slots.available_permits();
        let streaming = self.streaming.load(Ordering::Relaxed);
        ComputeGateStatus {
            admission: self.compute_admission,
            queue: self.compute_queue,
            in_flight,
            // Admitted minus running minus streaming. The three are read at slightly different
            // instants, so this can be off by one transiently.
            waiting: admitted.saturating_sub(in_flight).saturating_sub(streaming),
            streaming,
            shed_total: self.shed_total.load(Ordering::Relaxed),
        }
    }
}

/// Bounds concurrent `/control/ingest` handlers, each of which holds a blocking-pool thread
/// through decoding, term resolution, index reads and the receipt wait. Unbounded, ingest could
/// fill the pool and hang admitted viewports. The deny lane never shares that pool with ingest:
/// it has its own runtime. No queue and no timeout, so a refusal costs nothing. Per server rather
/// than process-global, since tests run many servers in one process.
pub struct IngestAdmission {
    pub bound: usize,
    permits: Arc<Semaphore>,
    /// Ingest 429s from this bound. A full write queue sheds inside the engine and is not counted
    /// here.
    shed_total: AtomicU64,
}

/// `/control/status`'s `ingest` block.
pub struct IngestAdmissionStatus {
    pub admission: usize,
    pub in_flight: usize,
    pub shed_total: u64,
}

impl IngestAdmission {
    pub fn new(bound: usize) -> Self {
        IngestAdmission {
            bound,
            permits: Arc::new(Semaphore::new(bound)),
            shed_total: AtomicU64::new(0),
        }
    }

    /// Takes a permit, or `None` at the bound. Move it into the blocking closure, which keeps its
    /// thread after a disconnected client's handler is dropped.
    pub fn try_admit(&self) -> Option<OwnedSemaphorePermit> {
        match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn status(&self) -> IngestAdmissionStatus {
        IngestAdmissionStatus {
            admission: self.bound,
            in_flight: self.bound - self.permits.available_permits(),
            shed_total: self.shed_total.load(Ordering::Relaxed),
        }
    }
}

/// At most one suggestion walk in flight per session. A second is shed with 429 at once rather
/// than queued, since a per-keystroke surface queued behind viewports would be unusable.
pub struct SuggestAdmission {
    in_flight: Arc<Mutex<FxHashSet<u64>>>,
}

impl SuggestAdmission {
    pub fn new() -> Self {
        SuggestAdmission {
            in_flight: Arc::new(Mutex::new(FxHashSet::default())),
        }
    }

    /// Starts a walk for this session, or `None` if one is already in flight.
    pub fn try_begin(&self, token_id: u64) -> Option<SuggestGuard> {
        let mut in_flight = self.in_flight.lock();
        if in_flight.insert(token_id) {
            Some(SuggestGuard {
                in_flight: Arc::clone(&self.in_flight),
                token_id,
            })
        } else {
            None
        }
    }
}

impl Default for SuggestAdmission {
    fn default() -> Self {
        Self::new()
    }
}

/// Releases the session's slot on drop, including when the handler is dropped mid-walk. Owns an
/// `Arc` so it can move into a `spawn_blocking` closure.
pub struct SuggestGuard {
    in_flight: Arc<Mutex<FxHashSet<u64>>>,
    token_id: u64,
}

impl Drop for SuggestGuard {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.token_id);
    }
}

/// The `[serve]` numbers and flags handlers read. [`Default`] is the shipped defaults.
#[derive(Debug, Clone)]
pub struct ServeLimits {
    pub max_k: usize,
    /// `/v1/categories`' page-size ceiling and its default. See `Config::max_category_values`.
    pub max_category_values: usize,
    /// The suggestion page ceiling and default `limit`. See `Config::max_suggestions`.
    pub max_suggestions: usize,
    /// The suggestion walk's budget. See `Config::max_suggestion_walk`.
    pub max_suggestion_walk: u64,
    /// The cardinality at or under which a suggestion walks the per-session set. See
    /// `Config::max_suggest_set_entities`.
    pub max_suggest_set_entities: u64,
    /// The publication vertex cap a shape is held to. See `Config::max_shape_vertices`.
    pub max_shape_vertices: u64,
    /// A `region` leaf's vertex cap. See `Config::max_region_vertices`.
    pub max_region_vertices: u64,
    /// A `region` leaf's boundary-cell budget, published beside it. See `Config::max_region_cells`.
    pub max_region_cells: usize,
    /// `POST /v1/artifacts/browse`'s page ceiling and default. See `Config::max_browse_rows`.
    pub max_browse_rows: usize,
    /// Row cap per `/control/ingest` request; over is 422, checked after the Arrow decode.
    pub ingest_max_batch_rows: usize,
    /// Buffered items at which `/control/ingest` answers 429. Distinct from `ingest_queue_bound`,
    /// which counts queued commands.
    pub ingest_buffer_max_items: usize,
    /// Body-byte cap per `/control/ingest` request; over is 422. Enforced by the route's
    /// `DefaultBodyLimit` layer, and held here so the refusal names the same number.
    pub ingest_max_batch_bytes: usize,
    /// Body-byte cap on `PUT` and `PATCH /control/layers/{name}/artifacts`; over is 422, enforced
    /// as `ingest_max_batch_bytes` is.
    pub publish_max_body_bytes: usize,
    /// Artifact records per publication; over is 422. See `Config::max_artifacts_per_request`.
    pub max_artifacts_per_request: usize,
    /// Members per growth page, summed over its artifacts; over is 422. See
    /// `Config::max_members_per_request`.
    pub max_members_per_request: usize,
    /// The published bound on an exclusion list. Published, not enforced: the field it bounds is
    /// not built.
    pub max_excluded_per_request: usize,
    /// The runtime half of the trailer's `stage_ns` switch; the `bench-timing` feature is the
    /// other, and both must be on.
    pub stage_timing: bool,
    /// The streamed viewport's flush threshold. See `Config::stream_flush_bytes`.
    pub stream_flush_bytes: usize,
    /// One frame send's stall budget. See `Config::stream_write_stall_ms`.
    pub stream_write_stall_ms: u64,
    /// The whole emit phase's wall budget. See `Config::stream_deadline_ms`.
    pub stream_deadline_ms: u64,
    /// A bulk read's rows per page. See `Config::max_page_rows`.
    pub max_page_rows: u32,
    /// A bulk read's Arrow bytes per page. See `Config::max_page_bytes`.
    pub max_page_bytes: usize,
    /// Bytes one bulk-read response may carry. See `Config::bulk_response_bytes`.
    pub bulk_response_bytes: usize,
    /// Time one bulk-read response may run. See `Config::bulk_response_ms`.
    pub bulk_response_ms: u64,
    /// `POST /v1/aggregate`'s ceilings on groupings, `top` and a named list, published in
    /// `/v1/meta`. See `Config::max_aggregate_groupings`.
    pub max_aggregate_groupings: u32,
    pub max_aggregate_top: u32,
    pub max_aggregate_named: u32,
    /// The most cells an aggregate cell level may list, published in `/v1/meta`. See
    /// `Config::max_aggregate_cells`.
    pub max_aggregate_cells: u64,
    /// An aggregate response's bytes and its pages' bytes. See `Config::aggregate_response_bytes`.
    pub aggregate_response_bytes: usize,
    pub aggregate_page_bytes: usize,
    /// `serve.dev_cors_origins`; empty means no CORS layer. See [`crate::cors`].
    pub dev_cors_origins: Vec<String>,
    /// `serve.cors_origins`, read by the viewer router only.
    pub cors_origins: Vec<String>,
    /// `serve.cors_loopback`: admit pages served from a loopback address, on the viewer plane only.
    pub cors_loopback: bool,
    /// `serve.visible_wait_max_secs`: the ceiling on a `wait=visible` acknowledgement's wait, which
    /// holds no lock and no executor.
    pub visible_wait_max_secs: u64,
}

impl ServeLimits {
    pub fn from_config(config: &tessera_config::Config) -> Self {
        ServeLimits {
            max_k: config.max_k,
            max_category_values: config.max_category_values,
            max_suggestions: config.max_suggestions,
            max_suggestion_walk: config.max_suggestion_walk,
            max_suggest_set_entities: config.max_suggest_set_entities,
            max_shape_vertices: config.max_shape_vertices,
            max_region_vertices: config.max_region_vertices,
            max_region_cells: config.max_region_cells,
            max_browse_rows: config.max_browse_rows,
            ingest_max_batch_rows: config.ingest_max_batch_rows,
            ingest_buffer_max_items: config.ingest_buffer_max_items,
            ingest_max_batch_bytes: config.ingest_max_batch_bytes,
            publish_max_body_bytes: config.publish_max_body_bytes,
            max_artifacts_per_request: config.max_artifacts_per_request,
            max_members_per_request: config.max_members_per_request,
            max_excluded_per_request: config.max_excluded_per_request,
            stage_timing: config.stage_timing,
            stream_flush_bytes: config.stream_flush_bytes,
            stream_write_stall_ms: config.stream_write_stall_ms,
            stream_deadline_ms: config.stream_deadline_ms,
            max_page_rows: config.max_page_rows,
            max_page_bytes: config.max_page_bytes,
            bulk_response_bytes: config.bulk_response_bytes,
            bulk_response_ms: config.bulk_response_ms,
            max_aggregate_groupings: config.max_aggregate_groupings,
            max_aggregate_top: config.max_aggregate_top,
            max_aggregate_named: config.max_aggregate_named,
            max_aggregate_cells: config.max_aggregate_cells,
            aggregate_response_bytes: config.aggregate_response_bytes,
            aggregate_page_bytes: config.aggregate_page_bytes,
            dev_cors_origins: config.dev_cors_origins.clone(),
            cors_origins: config.cors_origins.clone(),
            cors_loopback: config.cors_loopback,
            visible_wait_max_secs: config.visible_wait_max_secs,
        }
    }
}

impl Default for ServeLimits {
    fn default() -> Self {
        use tessera_config::defaults as c;
        ServeLimits {
            max_k: c::DEFAULT_MAX_K,
            max_category_values: c::DEFAULT_MAX_CATEGORY_VALUES,
            max_suggestions: c::DEFAULT_MAX_SUGGESTIONS,
            max_suggestion_walk: c::DEFAULT_MAX_SUGGESTION_WALK,
            max_suggest_set_entities: c::DEFAULT_MAX_SUGGEST_SET_ENTITIES,
            max_shape_vertices: tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES,
            max_region_vertices: c::DEFAULT_MAX_REGION_VERTICES,
            max_region_cells: tessera_engine::DEFAULT_MAX_REGION_CELLS,
            max_browse_rows: c::DEFAULT_MAX_BROWSE_ROWS,
            ingest_max_batch_rows: c::DEFAULT_INGEST_MAX_BATCH_ROWS,
            ingest_buffer_max_items: c::DEFAULT_INGEST_BUFFER_MAX_ITEMS,
            ingest_max_batch_bytes: c::DEFAULT_INGEST_MAX_BATCH_BYTES,
            publish_max_body_bytes: c::DEFAULT_PUBLISH_MAX_BODY_BYTES,
            max_artifacts_per_request: c::DEFAULT_MAX_ARTIFACTS_PER_REQUEST,
            max_members_per_request: c::DEFAULT_MAX_MEMBERS_PER_REQUEST,
            max_excluded_per_request: c::DEFAULT_MAX_EXCLUDED_PER_REQUEST,
            stage_timing: false,
            stream_flush_bytes: c::DEFAULT_STREAM_FLUSH_BYTES,
            stream_write_stall_ms: c::DEFAULT_STREAM_WRITE_STALL_MS,
            stream_deadline_ms: c::DEFAULT_STREAM_DEADLINE_MS,
            max_page_rows: c::DEFAULT_MAX_PAGE_ROWS,
            max_page_bytes: c::DEFAULT_MAX_PAGE_BYTES,
            bulk_response_bytes: c::DEFAULT_BULK_RESPONSE_BYTES,
            bulk_response_ms: c::DEFAULT_BULK_RESPONSE_MS,
            max_aggregate_groupings: c::DEFAULT_MAX_AGGREGATE_GROUPINGS,
            max_aggregate_top: c::DEFAULT_MAX_AGGREGATE_TOP,
            max_aggregate_named: c::DEFAULT_MAX_AGGREGATE_NAMED,
            max_aggregate_cells: c::DEFAULT_MAX_AGGREGATE_CELLS,
            aggregate_response_bytes: c::DEFAULT_AGGREGATE_RESPONSE_BYTES,
            aggregate_page_bytes: c::DEFAULT_AGGREGATE_PAGE_BYTES,
            dev_cors_origins: Vec::new(),
            cors_origins: Vec::new(),
            cors_loopback: false,
            visible_wait_max_secs: c::DEFAULT_VISIBLE_WAIT_MAX_SECS,
        }
    }
}

/// Process-wide server state, shared (behind `Arc`) across every axum handler on every plane.
pub struct AppState {
    pub engine: Engine,
    pub sessions: Mutex<SessionRegistry>,
    /// The allocator's trim cadence and gauges; see [`crate::memory`].
    pub heap: crate::memory::HeapWatch,
    /// The numbers and flags read from `[serve]`.
    pub limits: ServeLimits,
    /// At most one suggestion walk per session.
    pub suggest_admission: SuggestAdmission,
    /// The viewer/session admission gate. Never touched by the control plane.
    pub compute_gate: ComputeGate,
    /// Password checks at login, about one per core with a short queue, so a flood of argon2id
    /// work cannot take every blocking thread from admitted viewer and ingest requests.
    pub password_gate: ComputeGate,
    /// `POST /v1/items` and `POST /v1/artifacts` only. Runs under its own admission limit,
    /// `serve.bulk_admission`, so a long read takes no slot from the viewport and item routes. The
    /// compute threads and the memory cap are shared.
    pub bulk_gate: ComputeGate,
    /// The control plane's own bound, so ingest is never throttled by what viewports consume.
    pub ingest_admission: IngestAdmission,
    /// Principals, credentials, groups, grants and OIDC providers.
    pub catalogue: tessera_catalogue::Catalogue,
    /// Checks OIDC access tokens against their providers' published keys.
    pub oidc: crate::oidc::Verifier,
    /// Authenticates the superuser on the control plane.
    pub operator_credential: String,
    /// `[serve] request_log`, open for appending. `None` mounts no logging at all.
    pub request_log: Option<crate::request_log::RequestLog>,
    /// The write executor's fault switchboard, in the faults build only. The executor holds the
    /// same `Arc`, so `/control/faults/*` arms the thread that pauses.
    #[cfg(feature = "fault-injection")]
    pub faults: std::sync::Arc<tessera_lifecycle::faults::FaultSwitchboard>,
}

/// The token of an `Authorization: Bearer` header, on every plane.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// The viewer plane's authentication extractor: the bearer token, looked up by
/// [`AppState::authenticated_session`]. Handlers take it first after the state, and axum runs
/// extractors in order, so a bad token is refused before the path, query or body is read.
pub struct ViewerSession(pub Arc<Session>);

impl axum::extract::FromRequestParts<Arc<AppState>> for ViewerSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        let token = bearer_token(&parts.headers).ok_or(ApiError::BadCredential)?;
        let session = state.authenticated_session(token)?;
        crate::request_log::note_token_id(session.token_id());
        Ok(ViewerSession(session))
    }
}

/// A JSON request body. It is read only after the credential is checked, by the handler's
/// credential extractor on the viewer and session planes or by the control plane's middleware.
pub struct ApiJson<T>(pub T);

/// Why [`ApiJson`] refused a body.
pub enum ApiJsonRejection {
    /// JSON that is not the route's request object, as a `contract` refusal with serde's reason.
    Shape(ApiError),
    /// A body that is not JSON, not sent as JSON, or not read; answered as axum answers it.
    Body(axum::extract::rejection::JsonRejection),
}

impl axum::response::IntoResponse for ApiJsonRejection {
    fn into_response(self) -> axum::response::Response {
        match self {
            ApiJsonRejection::Shape(e) => e.into_response(),
            ApiJsonRejection::Body(rejection) => rejection.into_response(),
        }
    }
}

impl<T: serde::de::DeserializeOwned> axum::extract::FromRequest<Arc<AppState>> for ApiJson<T> {
    type Rejection = ApiJsonRejection;

    async fn from_request(
        req: axum::extract::Request,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiJsonRejection> {
        use axum::extract::rejection::JsonRejection;
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(ApiJson(value)),
            Err(JsonRejection::JsonDataError(e)) => {
                Err(ApiJsonRejection::Shape(ApiError::Contract(format!(
                    "{}; send the fields this route defines, with the values its schema allows",
                    e.body_text()
                ))))
            }
            Err(other) => Err(ApiJsonRejection::Body(other)),
        }
    }
}

/// A query string. One that does not deserialise is a `contract` refusal with serde's reason, as
/// [`ApiJson`] answers a wrongly shaped body.
pub struct ApiQuery<T>(pub T);

impl<T: serde::de::DeserializeOwned> axum::extract::FromRequestParts<Arc<AppState>> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(ApiQuery(value)),
            Err(e) => Err(ApiError::Contract(format!(
                "{}; send only the parameters this route defines, with the values its schema allows",
                e.body_text()
            ))),
        }
    }
}

impl AppState {
    /// Run `f` on the blocking pool behind the compute gate. The permits move into the closure, so
    /// they release when the work finishes, not when the caller stops waiting.
    pub async fn gated<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&AppState) -> Result<T, ApiError> + Send + 'static,
    {
        let (permits, _admission_us) = self.compute_gate.admit().await?;
        self.blocking(move |state| {
            let _permits = permits;
            f(state)
        })
        .await
    }

    /// Runs `f` on tokio's blocking pool, off the compute gate.
    pub async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&AppState) -> Result<T, ApiError> + Send + 'static,
    {
        let state = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&state))
            .await
            .map_err(crate::error::map_join_error)?
    }

    /// Runs the engine write `f` on tokio's blocking pool, off the compute gate, mapping a refusal
    /// by [`crate::error::map_accept_error`]. Deny changes never run here: the deny lane has its
    /// own runtime, so ingest filling this pool cannot delay a suppression.
    pub async fn write<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&AppState) -> Result<T, tessera_engine::AcceptError> + Send + 'static,
    {
        self.blocking(move |state| f(state).map_err(crate::error::map_accept_error))
            .await
    }

    /// Looks up a viewer token: unknown is 401, and ended or past its deadline is 403. The
    /// engine never checks a deadline, so this is the only deadline check.
    pub fn authenticated_session(&self, token: &str) -> Result<Arc<Session>, ApiError> {
        self.session_entry(token).map(|e| Arc::clone(&e.session))
    }

    /// [`Self::authenticated_session`], with what authorised the session.
    pub fn session_entry(&self, token: &str) -> Result<Arc<SessionEntry>, ApiError> {
        let entry = self
            .sessions
            .lock()
            .get(token)
            .map_err(|()| ApiError::ExpiredToken)?
            .ok_or(ApiError::BadCredential)?;
        if now_secs() >= entry.expires_at {
            return Err(ApiError::ExpiredToken);
        }
        Ok(entry)
    }

    /// Registers a freshly minted session if `admit` still allows it, under the registry's lock,
    /// and prunes what the insert's sweep removed. An entry `admit` refuses is never registered,
    /// and its token's projections are pruned. Returns whether it was registered.
    pub async fn register_session(
        self: &Arc<Self>,
        entry: SessionEntry,
        admit: impl FnOnce() -> bool,
    ) -> bool {
        let token_id = entry.session.token_id();
        let (registered, ended) = {
            let mut sessions = self.sessions.lock();
            if admit() {
                (true, sessions.insert(entry, now_secs()))
            } else {
                (false, vec![token_id])
            }
        };
        self.prune(ended).await;
        registered
    }

    /// Ends every live session `doomed` selects, prunes them, and returns how many ended.
    pub async fn end_sessions(self: &Arc<Self>, doomed: impl Fn(&SessionEntry) -> bool) -> usize {
        let ended = self.sessions.lock().end_where(doomed);
        let n = ended.len();
        self.prune(ended).await;
        n
    }

    /// Ends every session a catalogue change affected, prunes them on the calling thread, and
    /// returns how many. A change calls it on the blocking thread that committed it, straight
    /// after the commit, so its sessions end whether or not its caller is still waiting.
    pub fn end_affected_now(&self, affected: &tessera_catalogue::Affected) -> usize {
        if affected.is_empty() {
            return 0;
        }
        let ended = self.sessions.lock().end_where(|e| e.affected_by(affected));
        let n = ended.len();
        if n > 0 {
            self.engine.prune_tokens(&ended.into_iter().collect());
        }
        n
    }

    /// Drops the sessions `ended` names from the engine's caches, on a blocking thread, and
    /// returns once they are dropped. Every way a session ends reaches this, through
    /// [`Self::end_sessions`] or the sweep in [`Self::register_session`], after the registry
    /// has made the session unusable: pruning first would let a request that authenticated
    /// before the end publish its key again.
    async fn prune(self: &Arc<Self>, ended: Vec<u64>) {
        if ended.is_empty() {
            return;
        }
        let doomed: FxHashSet<u64> = ended.into_iter().collect();
        let pruner = Arc::clone(self);
        if let Err(e) =
            tokio::task::spawn_blocking(move || pruner.engine.prune_tokens(&doomed)).await
        {
            tracing::error!(error = %e, "pruning ended sessions from the engine's caches failed");
        }
    }
}

#[cfg(test)]
mod session_registry_tests {
    use super::*;

    /// The sweep's bounds are properties of this expression. A `SessionRegistry` cannot be built in
    /// a unit test, so the sweep itself is tested over HTTP in `tests/http_engine_state.rs`.
    #[test]
    fn the_sweep_threshold_doubles_above_the_floor_and_never_falls_below_it() {
        assert_eq!(next_sweep_threshold(0), SWEEP_FLOOR_ENTRIES);
        assert_eq!(next_sweep_threshold(1), SWEEP_FLOOR_ENTRIES);
        // The floor binds up to half itself; above that, doubling does.
        assert_eq!(
            next_sweep_threshold(SWEEP_FLOOR_ENTRIES / 2),
            SWEEP_FLOOR_ENTRIES
        );
        assert_eq!(next_sweep_threshold(100), 200);

        // A huge live set must not wrap to a small threshold.
        assert_eq!(next_sweep_threshold(usize::MAX), usize::MAX);
    }

    /// The sweep returns exactly the ids whose sessions went, and the index keeps the rest.
    #[test]
    fn the_sweep_returns_the_ids_it_dropped_and_keeps_the_rest() {
        let mut index: FxHashMap<u64, String> =
            (1..=4).map(|id| (id, format!("token-{id}"))).collect();
        let live = ["token-2", "token-4"];

        let mut dropped = prune_index(&mut index, |token| live.contains(&token));
        dropped.sort_unstable();

        assert_eq!(dropped, vec![1, 3]);
        let mut kept: Vec<u64> = index.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![2, 4]);
    }

    /// A sweep that removed nothing returns nothing, so no prune is started.
    #[test]
    fn a_sweep_that_removes_nothing_returns_nothing() {
        let mut index: FxHashMap<u64, String> =
            (1..=3).map(|id| (id, format!("token-{id}"))).collect();
        assert!(prune_index(&mut index, |_| true).is_empty());
        assert_eq!(index.len(), 3);
    }
}

#[cfg(test)]
mod compute_gate_tests {
    use super::*;

    /// With no queue, a second admit while the first is held sheds at once.
    #[tokio::test]
    async fn a_second_admit_sheds_immediately_when_slots_are_exhausted() {
        let gate = ComputeGate::new(1, 0, 250);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");

        let second = gate.admit().await;
        assert!(
            matches!(second, Err(ApiError::Backpressure {
                cause: crate::error::ShedCause::ComputeGate,
                ..
            })),
            "a saturated gate must shed the second admission"
        );
        assert_eq!(gate.status().shed_total, 1);

        drop(first_permits);
    }

    /// With a queue slot free but compute held, a second admit sheds after the timeout.
    #[tokio::test]
    async fn a_queued_admit_sheds_after_the_admission_timeout() {
        let gate = ComputeGate::new(1, 1, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");

        let second = gate.admit().await;
        assert!(
            matches!(second, Err(ApiError::Backpressure {
                cause: crate::error::ShedCause::ComputeGate,
                ..
            })),
            "a caller that cannot get a compute permit within the timeout must be shed"
        );
        assert_eq!(gate.status().shed_total, 1);

        drop(first_permits);
    }

    /// A shed leaks no permit: once the holder releases, the next admit succeeds.
    #[tokio::test]
    async fn no_permit_leak_after_a_shed() {
        let gate = ComputeGate::new(1, 0, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");
        assert!(matches!(gate.admit().await, Err(ApiError::Backpressure {
                cause: crate::error::ShedCause::ComputeGate,
                ..
            })));

        drop(first_permits);

        let third = gate.admit().await;
        assert!(
            third.is_ok(),
            "a permit leak would make this admit shed too"
        );
    }

    /// A compute-timeout shed returns its slot permit.
    #[tokio::test]
    async fn no_slot_leak_on_a_compute_timeout_shed() {
        let gate = ComputeGate::new(1, 1, 1);
        let (first_permits, _) = gate.admit().await.expect("first admit must succeed");
        assert!(matches!(gate.admit().await, Err(ApiError::Backpressure {
                cause: crate::error::ShedCause::ComputeGate,
                ..
            })));

        // The timed-out attempt is not counted as waiting.
        let slots_status = gate.status();
        assert_eq!(
            slots_status.waiting, 0,
            "the timed-out attempt must not remain counted as waiting"
        );

        drop(first_permits);
        let fourth = gate.admit().await;
        assert!(fourth.is_ok(), "a slot leak would make this admit shed too");
    }

    /// The password gate admits its bound and as many waiting, and sheds past that at once with
    /// its own cause.
    #[tokio::test]
    async fn the_password_gate_sheds_past_its_bound_and_queue() {
        let gate = ComputeGate::for_passwords(1, 60_000);
        let (held, _) = gate.admit().await.expect("the first check is admitted");
        let waiting = tokio::spawn({
            let slots = Arc::clone(&gate.slots);
            async move { slots.available_permits() }
        });
        assert_eq!(waiting.await.unwrap(), 1, "one slot is left for the queue");
        let queued = Arc::clone(&gate.slots).try_acquire_owned().unwrap();
        assert!(matches!(
            gate.admit().await,
            Err(ApiError::Backpressure {
                cause: crate::error::ShedCause::PasswordGate,
                ..
            })
        ));
        drop((held, queued));
        assert!(gate.admit().await.is_ok());
    }

    /// `in_flight` and `waiting` follow an admitted permit and return to zero on release.
    #[tokio::test]
    async fn status_reports_in_flight_and_resets_on_release() {
        let gate = ComputeGate::new(2, 1, 250);
        let status = gate.status();
        assert_eq!(status.admission, 2);
        assert_eq!(status.queue, 1);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.waiting, 0);

        let (permits, _) = gate.admit().await.expect("admit must succeed");
        let status = gate.status();
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.waiting, 0);

        drop(permits);
        let status = gate.status();
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.waiting, 0);
    }
}
