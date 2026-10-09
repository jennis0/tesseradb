//! Where a level's figures are held: the counts over each fragment's base rows, the deny
//! corrections and the tails, and the walk that fills the first of these.
//!
//! # Single flight, removal only, and a byte bound
//!
//! Concurrent requests for one key share one build ([`mosaica_cache::SingleFlightCache`]). A
//! request waits for another's build for as long as its client stays connected, up to
//! [`BUILD_WAIT_MS`]. A build on a large view takes longer than `serve.single_flight_wait_ms`,
//! and a viewport has sent its head before it reads the figures, so a refusal at that budget would
//! cut a response that the build was about to complete. At most [`CONCURRENT_BUILDS`] walks run at
//! once, each holding an accumulator per worker of the count pool. Eviction only removes, and a
//! refill walks the same rows over the same column, so the bound decides what is resident and
//! never what is served. An entry larger than the whole bound is handed to its caller and not
//! admitted.
//!
//! # A walk gives way to points
//!
//! A walk reads every row it counts, which on a corpus larger than memory streams from disk, and a
//! viewport's reads queue behind it. So a walk waits between chunks while any viewport is drawing
//! points ([`FiguresCache::drawing`]): from the start of its sweep to its last point, less the time
//! it is blocked handing a frame to its client. A walk gives way for at most
//! `serve.masked_count_give_way_ms` from its first wait, and not at all while a drawing request is
//! itself waiting on a walk, since that request's points wait on the walks. A request is not
//! counted as drawing while it waits on a walk, its own included, so nothing waits on itself. A
//! walk, or a walk waiting for a place, that every caller has left stops and holds nothing.
//!
//! The bound is `serve.masked_count_cache_bytes`. An entry over a fragment's base rows is 4 B an
//! artifact for counts alone and 40 B with a centroid's sums and a box. A layer that serves a box
//! keeps a reserve beside it as an entry of its own, 4 B an artifact and 128 B more for each
//! artifact with more than sixteen placed rows, so an entry too large for the bound loses its
//! reserve and keeps its counts. A field's tally over a fragment's base rows is under a kilobyte,
//! held in a cache of its own under a bound of the same size.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use mosaica_cache::{CacheWeight, Cancel, SingleFlightCache, WaitEnded, WaitingBuildError};

use super::counts::{CountsAt, Deltas, Reserves};
use super::denied::DenyCorrection;
use super::field::{FieldDeny, FieldLeft, Held};
use super::labels::{HeldLabels, LabelStore};
use super::worker::Worker;
use super::Geometry;

/// How many builds may walk at once.
const CONCURRENT_BUILDS: usize = 2;

/// How long a build gives way to drawing requests, from its first wait, unless
/// `serve.masked_count_give_way_ms` says otherwise.
pub const DEFAULT_GIVE_WAY_MS: u64 = 2_000;

/// How long a connected request waits for another's build before it is refused. Far beyond any
/// build measured, so it ends a wait only on a build that has stopped making progress.
const BUILD_WAIT_MS: u64 = 600_000;

/// The bound on the deny corrections and tails held, which are sparse: one entry per artifact a
/// denied or a tail row touches.
const CORRECTIONS_BYTES: u64 = 256 << 20;

/// What one held count is a function of.
///
/// Named fields rather than a tuple: several of the terms are integers of one type, and a
/// transposition at the construction site would compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FiguresKey {
    /// A level's counts over one fragment's base rows ([`FragmentKey`]).
    Fragment(FragmentKey),
    /// A level's counts over one request's whole composed mask: the walk of the whole composed
    /// mask, the answer while a level's denied-row labels are being read.
    Exact(ExactKey),
    /// A field's tally over one fragment's base rows ([`FieldKey`]).
    Field(FieldKey),
    /// A field's tally over a fragment's base rows less one deny's, walked where the deny
    /// subtracts every extreme value the fragment's tally keeps on a side.
    FieldLeft(FieldDenyKey),
}

/// A field's tally over a fragment's base rows: a function of the fragment's base rows and their
/// values, which nothing changes between folds.
///
/// - `terms`, `identity` and `view`: as [`FragmentKey`]'s.
/// - `column`: the field's column as the request resolved it.
/// - `kind`: how its values are compared, which a field declared again with another type moves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldKey {
    pub terms: [u8; 32],
    pub identity: [u8; 32],
    pub view: String,
    pub column: String,
    pub kind: u8,
}

/// What a field's deny correction is a function of: its fragment's tally and the denied base
/// rows, as [`DenyKey`] names them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldDenyKey {
    pub field: FieldKey,
    pub deny_version: u64,
    pub failing: Option<u64>,
}

/// What one session's tail of a field is a function of: its rows above the fragment's base rows,
/// as [`TailKey`] names them, and their values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldTailKey {
    pub token_id: u64,
    pub view: String,
    pub column: String,
    pub kind: u8,
    pub segments_version: u64,
    pub projection_segments_version: u64,
    pub overlay_version: u64,
}

/// The fragment's counts: a function of the fragment's base rows and the column's base labels.
///
/// - `terms`: the session's grant ([`crate::Session::terms_digest`]).
/// - `identity`: the bundle identity the fragment was unioned against. A fold rotates it, and
///   nothing else renumbers the base rows or rewrites the postings below them.
/// - `view`, `layer`, `level`: what the column is of.
/// - `column`: [`crate::row_column::RowColumn::identity`], which a column composed, opened,
///   recomposed or claimed afresh moves. Within one, a growth or a publication is followed by its
///   steps, not by a new key.
/// - `geometry`: what the entry carries beside the counts.
///
/// Not in the key: the overlay's version, the segments' versions and the fragment's watermark. A
/// flush adds rows above the base only, a deny is corrected per request, and an item flushed under
/// a key the grant holds has a row above the base.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FragmentKey {
    pub terms: [u8; 32],
    pub identity: [u8; 32],
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub column: u64,
    pub geometry: Geometry,
}

/// One request's composed mask, named by every input to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ExactKey {
    pub terms: [u8; 32],
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub level_version: u64,
    pub geometry: Geometry,
    pub segments_version: u64,
    pub projection_segments_version: u64,
    pub overlay_version: u64,
    pub fragment_identity: [u8; 32],
    pub fragment_watermark: u64,
}

/// What a deny correction is a function of: the fragment, the level's labels at one version of
/// one column, and the denied base rows. `failing` is the overlay's version where the mask also
/// subtracts buffered rows the viewer fails below the base, which no deny version follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DenyKey {
    pub terms: [u8; 32],
    pub identity: [u8; 32],
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub column: u64,
    pub level_version: u64,
    pub deny_version: u64,
    pub failing: Option<u64>,
}

/// What one session's tail is a function of: its rows above the fragment's base rows and the
/// labels the column gives them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TailKey {
    pub token_id: u64,
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub column: u64,
    pub level_version: u64,
    pub segments_version: u64,
    pub projection_segments_version: u64,
    pub overlay_version: u64,
}

/// Everything about one request's composed mask that a cache keyed on the mask is a function of,
/// gathered once per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaskIdentity {
    /// The session's process-local identity, for the caches held per session.
    pub token_id: u64,
    /// The session's satisfied term set ([`crate::Session::terms_digest`]), for the caches every
    /// session holding that term set shares.
    pub terms: [u8; 32],
    pub segments_version: u64,
    /// The generation the session's row projection was built at, one behind `segments_version`
    /// while a stale projection is served.
    pub projection_segments_version: u64,
    pub overlay_version: u64,
    pub fragment_identity: [u8; 32],
    pub fragment_watermark: u64,
}

impl MaskIdentity {
    /// This mask's exact key for one level.
    pub(crate) fn exact_key(
        &self,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        geometry: Geometry,
    ) -> ExactKey {
        ExactKey {
            terms: self.terms,
            view: view.to_string(),
            layer: layer.to_string(),
            level,
            level_version,
            geometry,
            segments_version: self.segments_version,
            projection_segments_version: self.projection_segments_version,
            overlay_version: self.overlay_version,
            fragment_identity: self.fragment_identity,
            fragment_watermark: self.fragment_watermark,
        }
    }
}

/// How many versions of one fragment's counts are kept: a request still holding a level's form
/// from before a publication reads the counts at that form's version.
const VERSIONS_KEPT: usize = 4;

/// What one held reserve belongs to: a fragment's entry, and the version its walk read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReserveKey {
    pub fragment: FragmentKey,
    pub filled_at: u64,
}

/// Where an entry is written, and when it was last.
pub(crate) struct Written {
    pub(super) dir: std::path::PathBuf,
    pub(super) stem: String,
    /// The newest version written to disk.
    pub(super) at: AtomicU64,
    pub(super) last: Mutex<Option<Instant>>,
    pub(super) writing: std::sync::atomic::AtomicBool,
}

/// One held entry: the counts over a fragment's base rows at the last few versions of the level
/// they were brought to, newest last.
pub(crate) struct FragmentCounts {
    versions: Mutex<std::collections::VecDeque<Arc<CountsAt>>>,
    /// What was charged against the bound when the entry was admitted.
    weight: u64,
    /// `None` for an entry nothing writes: a walk of a whole mask.
    pub(super) written: Option<Written>,
}

impl FragmentCounts {
    /// `written` names where the entry is written, and that its version is on disk already.
    pub(crate) fn new(counts: CountsAt, written: Option<(std::path::PathBuf, String)>) -> Self {
        let weight = counts.weight_bytes();
        let at = counts.at;
        FragmentCounts {
            versions: Mutex::new(std::iter::once(Arc::new(counts)).collect()),
            weight,
            written: written.map(|(dir, stem)| Written {
                dir,
                stem,
                at: AtomicU64::new(at),
                last: Mutex::new(Some(Instant::now())),
                writing: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// The counts at `version`: held, or brought forward from an older version held by `follow`,
    /// which is handed that version and answers `None` where it cannot reach `version` from it.
    pub(crate) fn at(
        &self,
        version: u64,
        follow: impl FnOnce(&CountsAt) -> Option<CountsAt>,
    ) -> Option<Arc<CountsAt>> {
        let mut versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = versions.iter().find(|held| held.at == version) {
            return Some(Arc::clone(held));
        }
        let from = versions.iter().rev().find(|held| held.at < version)?;
        let followed = Arc::new(follow(from)?);
        let at = versions.partition_point(|held| held.at < version);
        versions.insert(at, Arc::clone(&followed));
        while versions.len() > VERSIONS_KEPT {
            versions.pop_front();
        }
        Some(followed)
    }

    /// The newest version held.
    pub(crate) fn newest(&self) -> Arc<CountsAt> {
        let versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            versions
                .back()
                .expect("an entry holds at least one version"),
        )
    }
}

impl CacheWeight for FragmentCounts {
    fn cache_weight_bytes(&self) -> u64 {
        self.weight
    }
}

impl CacheWeight for DenyCorrection {
    fn cache_weight_bytes(&self) -> u64 {
        self.weight_bytes()
    }
}

impl CacheWeight for Reserves {
    fn cache_weight_bytes(&self) -> u64 {
        self.weight_bytes()
    }
}

/// A tail's corrections, by artifact.
pub(crate) struct Tail(pub(crate) Deltas);

impl CacheWeight for Tail {
    fn cache_weight_bytes(&self) -> u64 {
        super::counts::deltas_weight(&self.0)
    }
}

/// `(view, layer, level)`.
pub(crate) type LevelAddress = (String, String, u32);

/// The gauges an operator reads. Names no artifact and no principal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FiguresStats {
    /// Requests answered from a held entry, or from another request's build of it.
    pub hits: u64,
    /// Entries started: a walk, a read from disk or an exact walk.
    pub misses: u64,
    pub evictions: u64,
    /// Bytes currently held.
    pub resident_bytes: u64,
    /// Entries currently held, and builds in flight.
    pub entries: usize,
    /// Requests waiting at this instant for another request's build of their key.
    pub waiters: u64,
    /// Walks of a fragment's base rows for a level.
    pub fills: u64,
    /// Walks of a fragment's base rows for a field, which a field the base stores no tallies of
    /// takes, and so does one whose composed tallies do not cover the fragment's base rows.
    pub field_fills: u64,
    /// Composed field tallies that did not cover the fragment's base rows, or a deny that
    /// subtracted more than they held: each answered by a walk instead.
    pub field_mismatches: u64,
    /// Fragments' counts read back from disk rather than walked.
    pub loads: u64,
    /// Walks of a whole composed mask, taken where a level's denied-row labels are not yet held.
    pub exact: u64,
    /// Boxes worked out from an artifact's rows, or a field's base rows less a deny walked,
    /// because a deny reached a side its reserve could not answer.
    pub reserve_spent: u64,
    /// Entries, counts or reserves, larger than the whole bound: served to the request that built
    /// them and not kept.
    pub not_admitted: u64,
    /// Bytes of denied-row labels held, and their bound.
    pub labels_bytes: u64,
    pub labels_bound_bytes: u64,
    /// Denied rows whose labels were read from a level's column.
    pub labels_rows_read: u64,
    /// Bytes of figures in the cache directory, and their bound.
    pub disk_bytes: u64,
    pub disk_bound_bytes: u64,
}

/// The counters the figures keep, shared with what outlives a request.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) fills: AtomicU64,
    pub(crate) field_fills: AtomicU64,
    pub(crate) field_mismatches: AtomicU64,
    pub(crate) loads: AtomicU64,
    pub(crate) exact: AtomicU64,
    pub(crate) spent: AtomicU64,
}

/// One request's part in [`FiguresCache::drawing`].
#[derive(Debug, Default)]
pub(crate) struct DrawingTurn {
    state: Mutex<TurnState>,
}

#[derive(Debug, Default, Clone, Copy)]
struct TurnState {
    /// Between the start of the request's sweep and its last point.
    drawing: bool,
    /// The request's calls waiting on a build, its own builds included.
    waiting: usize,
    /// Blocked handing a frame to its client.
    sending: bool,
}

impl TurnState {
    /// A build gives way to this request.
    fn draws(self) -> bool {
        self.drawing && self.waiting == 0 && !self.sending
    }

    /// This request's points wait on a build, so no build gives way.
    fn waits_to_draw(self) -> bool {
        self.drawing && self.waiting > 0
    }
}

/// What the builds give way to, across every request.
#[derive(Debug, Default)]
struct Drawing {
    /// Requests that [`TurnState::draws`].
    draw: usize,
    /// Requests that [`TurnState::waits_to_draw`].
    wait_to_draw: usize,
}

/// The requests waiting for one key's build, the builder included, each by its cancellation.
/// `None` stands for a caller with no client to lose.
#[derive(Debug, Default)]
struct Interest {
    callers: Mutex<Vec<(u64, Option<crate::CancelToken>)>>,
}

impl Interest {
    /// Every caller that wanted this build has gone.
    fn abandoned(&self) -> bool {
        let callers = self.callers.lock().unwrap_or_else(PoisonError::into_inner);
        callers.iter().all(|(_, cancel)| {
            cancel
                .as_ref()
                .is_some_and(crate::CancelToken::is_cancelled)
        })
    }
}

/// How often a build parked for a place, or giving way, looks at whether anyone still wants it.
const ABANDON_TICK: Duration = Duration::from_millis(20);

/// One build in flight, handed to the walk.
pub(crate) struct Build<'a> {
    cache: &'a FiguresCache,
    interest: &'a Interest,
    /// When this build first gave way. It gives way for at most the cache's `give_way` after it.
    first_wait: OnceLock<Instant>,
}

impl Build<'_> {
    /// Returns once no request is drawing points, or this build has given way for its budget, or
    /// some drawing request is waiting on a build. `false` when every caller that wanted this
    /// build has gone, and the walk should stop. A build calls it between chunks of its walk.
    ///
    /// Never called from inside a `self.pool` job: a pool worker held here could be the one a
    /// drawing request's sweep is waiting for.
    pub(crate) fn give_way(&self) -> bool {
        let budget = Duration::from_millis(self.cache.give_way_ms.load(Ordering::Relaxed));
        let mut drawing = self
            .cache
            .drawing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.interest.abandoned() {
                return false;
            }
            if drawing.draw == 0 || drawing.wait_to_draw > 0 {
                return true;
            }
            let now = Instant::now();
            let until = *self.first_wait.get_or_init(|| now) + budget;
            if now >= until {
                return true;
            }
            drawing = self
                .cache
                .drawn
                .wait_timeout(drawing, (until - now).min(ABANDON_TICK))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A walk stopped because every caller that wanted it had gone.
struct Abandoned;

/// The cache itself, under a byte bound.
pub struct FiguresCache {
    slots: SingleFlightCache<FiguresKey, FragmentCounts>,
    /// The reserves, beside the counts they were walked with.
    pub(super) reserves: SingleFlightCache<ReserveKey, Reserves>,
    /// The deny corrections, per fragment and deny version.
    pub(super) denies: SingleFlightCache<DenyKey, DenyCorrection>,
    /// The tails, per session and generation.
    pub(super) tails: SingleFlightCache<TailKey, Tail>,
    /// The fields' tallies over each fragment's base rows, and over what a deny leaves of them,
    /// under a bound of the same size as `slots`'.
    pub(super) fields: SingleFlightCache<FiguresKey, Held>,
    /// Per field, the newest tally of the base rows a deny leaves, which a later deny subtracting
    /// every row that one did may read before it walks; under the corrections' bound.
    pub(super) field_left: SingleFlightCache<FieldKey, FieldLeft>,
    /// The fields' deny corrections, per fragment and deny version.
    pub(super) field_denies: SingleFlightCache<FieldDenyKey, FieldDeny>,
    /// The fields' tails, per session and generation.
    pub(super) field_tails: SingleFlightCache<FieldTailKey, Held>,
    /// Per `(view, layer, level)`, the labels of the view's denied base rows.
    pub(super) labels: LabelStore,
    /// Where entries are persisted: one directory per bundle identity beneath it. `None` holds
    /// nothing on disk.
    pub(super) dir: Option<std::path::PathBuf>,
    /// The bytes `dir` may hold.
    pub(super) disk_bound: Arc<AtomicU64>,
    /// The one thread that writes entries and reads denied rows' labels in the background.
    pub(super) worker: Worker,
    /// Builds walking now, at most [`CONCURRENT_BUILDS`].
    building: Mutex<usize>,
    built: Condvar,
    drawing: Mutex<Drawing>,
    drawn: Condvar,
    /// How long a build gives way, from its first wait (`serve.masked_count_give_way_ms`).
    give_way_ms: AtomicU64,
    /// Who wants each key's build.
    interest: Mutex<FxHashMap<FiguresKey, Arc<Interest>>>,
    next_caller: AtomicU64,
    pub(super) counters: Arc<Counters>,
}

/// The bytes of figures the cache directory holds unless `serve.figures_disk_bytes` says
/// otherwise.
pub const DEFAULT_DISK_BYTES: u64 = 8 << 30;

impl Default for FiguresCache {
    fn default() -> Self {
        Self::new(u64::MAX, None)
    }
}

impl std::fmt::Debug for FiguresCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FiguresCache")
            .field("stats", &self.stats())
            .finish()
    }
}

impl FiguresCache {
    /// `bound_bytes` is the resident-byte ceiling. `u64::MAX` means no bound, which every
    /// construction site outside the server has until [`Self::set_bound_bytes`] is called. `dir`
    /// is where entries are persisted, in the engine's cache directory and never in the bundle.
    pub fn new(bound_bytes: u64, dir: Option<std::path::PathBuf>) -> Self {
        let slots = SingleFlightCache::new(bound_bytes);
        slots.set_wait_budget_ms(BUILD_WAIT_MS);
        let disk_bound = Arc::new(AtomicU64::new(DEFAULT_DISK_BYTES));
        let reserves = SingleFlightCache::new(bound_bytes);
        let denies = SingleFlightCache::new(CORRECTIONS_BYTES);
        denies.set_wait_budget_ms(BUILD_WAIT_MS);
        let tails = SingleFlightCache::new(CORRECTIONS_BYTES);
        tails.set_wait_budget_ms(BUILD_WAIT_MS);
        let fields = SingleFlightCache::new(bound_bytes);
        fields.set_wait_budget_ms(BUILD_WAIT_MS);
        let field_denies = SingleFlightCache::new(CORRECTIONS_BYTES);
        field_denies.set_wait_budget_ms(BUILD_WAIT_MS);
        let field_tails = SingleFlightCache::new(CORRECTIONS_BYTES);
        field_tails.set_wait_budget_ms(BUILD_WAIT_MS);
        FiguresCache {
            slots,
            reserves,
            denies,
            tails,
            fields,
            field_left: SingleFlightCache::new(CORRECTIONS_BYTES),
            field_denies,
            field_tails,
            labels: LabelStore {
                held: Arc::new(HeldLabels::new(super::labels::DEFAULT_LABELS_BYTES)),
                dir: dir.clone(),
                disk_bound: Arc::clone(&disk_bound),
            },
            dir,
            disk_bound,
            worker: Worker::default(),
            building: Mutex::new(0),
            built: Condvar::new(),
            drawing: Mutex::new(Drawing::default()),
            drawn: Condvar::new(),
            give_way_ms: AtomicU64::new(DEFAULT_GIVE_WAY_MS),
            interest: Mutex::new(FxHashMap::default()),
            next_caller: AtomicU64::new(0),
            counters: Arc::default(),
        }
    }

    /// Move the ceiling, and drop what is held so the memory comes back at once rather than at
    /// the next build.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.slots.set_bound_bytes(bound_bytes);
        self.slots.retain_keys(|_| false);
        self.reserves.set_bound_bytes(bound_bytes);
        self.reserves.retain_keys(|_| false);
        self.fields.set_bound_bytes(bound_bytes);
        self.fields.retain_keys(|_| false);
    }

    /// The bytes of figures the cache directory may hold.
    pub fn set_disk_bound_bytes(&self, bytes: u64) {
        self.disk_bound.store(bytes, Ordering::Relaxed);
        if let Some(root) = self.dir.clone() {
            self.worker
                .submit(move || super::persist::hold_under(&root, bytes));
        }
    }

    /// How long a build gives way to drawing requests, from its first wait.
    pub fn set_give_way_ms(&self, give_way_ms: u64) {
        self.give_way_ms.store(give_way_ms, Ordering::Relaxed);
        self.drawn.notify_all();
    }

    pub fn stats(&self) -> FiguresStats {
        let stats = self.slots.stats();
        let fields = self.fields.stats();
        let labels = self.labels.held.stats();
        FiguresStats {
            hits: stats.hits + fields.hits,
            misses: stats.misses + fields.misses,
            evictions: stats.evictions + fields.evictions,
            resident_bytes: stats.bytes + self.reserves.stats().bytes + fields.bytes,
            entries: stats.entries + fields.entries,
            waiters: stats.waiters_now + fields.waiters_now,
            fills: self.counters.fills.load(Ordering::Relaxed),
            field_fills: self.counters.field_fills.load(Ordering::Relaxed),
            field_mismatches: self.counters.field_mismatches.load(Ordering::Relaxed),
            loads: self.counters.loads.load(Ordering::Relaxed),
            exact: self.counters.exact.load(Ordering::Relaxed),
            reserve_spent: self.counters.spent.load(Ordering::Relaxed),
            not_admitted: stats.oversized_admissions
                + self.reserves.stats().oversized_admissions
                + fields.oversized_admissions,
            labels_bytes: labels.bytes,
            labels_bound_bytes: labels.bound,
            labels_rows_read: labels.rows_read,
            disk_bytes: self.dir.as_deref().map_or(0, super::persist::bytes_under),
            disk_bound_bytes: self.disk_bound.load(Ordering::Relaxed),
        }
    }

    /// Remove from the cache directory every bundle identity's entries but `identity`'s: at open,
    /// and whenever a compaction rotates the identity.
    pub(crate) fn sweep(&self, identity: [u8; 32]) {
        self.field_left.retain_keys(|key| key.identity == identity);
        if let Some(root) = self.dir.clone() {
            self.worker
                .submit(move || super::persist::sweep_other_identities(&root, &identity));
        }
    }

    /// The reserve walked with `key`'s counts at `filled_at`, where it is held.
    pub(crate) fn reserve(&self, key: &FragmentKey, filled_at: u64) -> Option<Arc<Reserves>> {
        let key = ReserveKey {
            fragment: key.clone(),
            filled_at,
        };
        match self.reserves.peek(&key) {
            mosaica_cache::Peek::Ready(held) => Some(held),
            _ => None,
        }
    }

    /// Hold `reserves` for `key`'s counts at `filled_at`, unless it is larger than the bound.
    pub(crate) fn hold_reserve(&self, key: &FragmentKey, filled_at: u64, reserves: Reserves) {
        let key = ReserveKey {
            fragment: key.clone(),
            filled_at,
        };
        let _ = self.reserves.get_or_derive(key, None, |_| reserves);
    }

    /// Every entry's newest counts not yet on disk: what a clean shutdown writes.
    fn unwritten(&self) -> Vec<(FiguresKey, Arc<FragmentCounts>, Arc<CountsAt>)> {
        self.slots
            .ready_entries()
            .into_iter()
            .filter_map(|(key, held)| {
                let written = held.written.as_ref()?;
                let newest = held.newest();
                (written.at.load(Ordering::Acquire) < newest.at).then_some((
                    key,
                    Arc::clone(&held),
                    newest,
                ))
            })
            .collect()
    }

    /// Wait until the background work queued so far has run.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn settle(&self) {
        self.worker.drain();
    }

    /// Drop the tails held for these sessions.
    pub(crate) fn prune_tokens(&self, token_ids: &rustc_hash::FxHashSet<u64>) {
        self.tails
            .retain_keys(|key| !token_ids.contains(&key.token_id));
        self.field_tails
            .retain_keys(|key| !token_ids.contains(&key.token_id));
    }

    /// This key's counts, building them if nothing is held. A request arriving while another
    /// builds the same key waits for that build, while `cancel` holds and up to
    /// [`BUILD_WAIT_MS`], and is handed its result. `turn`'s request is not counted as drawing
    /// for the duration. A build that every caller has left stops, holds nothing, and its
    /// callers are answered [`WaitEnded::Cancelled`].
    pub(crate) fn get_or_build(
        &self,
        key: FiguresKey,
        turn: &DrawingTurn,
        cancel: Option<&crate::CancelToken>,
        walk: impl FnOnce(&Build<'_>) -> Option<FragmentCounts>,
    ) -> Result<Arc<FragmentCounts>, WaitEnded> {
        self.get_or_build_in(&self.slots, key, turn, cancel, walk)
    }

    /// [`Self::get_or_build`] into `slots`, which holds one kind of entry: a level's counts or a
    /// field's tally. Every kind shares the places to walk, the giving way and the callers' interest.
    pub(crate) fn get_or_build_in<V: CacheWeight + Send + Sync + 'static>(
        &self,
        slots: &SingleFlightCache<FiguresKey, V>,
        key: FiguresKey,
        turn: &DrawingTurn,
        cancel: Option<&crate::CancelToken>,
        walk: impl FnOnce(&Build<'_>) -> Option<V>,
    ) -> Result<Arc<V>, WaitEnded> {
        let _waiting = self.turn_guard(turn, |t| t.waiting += 1, |t| t.waiting -= 1);
        let (interest, _caller) = self.register(&key, cancel);
        let polled: &dyn Cancel = match cancel {
            Some(cancel) => cancel,
            None => &mosaica_cache::NeverCancelled,
        };
        slots
            .get_or_try_build_waiting(key, polled, || {
                let _permit = self.build_permit(&interest).ok_or(Abandoned)?;
                walk(&Build {
                    cache: self,
                    interest: &interest,
                    first_wait: OnceLock::new(),
                })
                .ok_or(Abandoned)
            })
            .map_err(|ended| match ended {
                WaitingBuildError::Wait(ended) => ended,
                WaitingBuildError::Build(Abandoned) => WaitEnded::Cancelled,
            })
    }

    /// Counts `turn`'s request as drawing points until the guard drops.
    pub(crate) fn drawing<'a>(&'a self, turn: &'a DrawingTurn) -> impl Drop + 'a {
        self.turn_guard(turn, |t| t.drawing = true, |t| t.drawing = false)
    }

    /// Does not count `turn`'s request as drawing until the guard drops: it is blocked on its
    /// client.
    pub(crate) fn sending<'a>(&'a self, turn: &'a DrawingTurn) -> impl Drop + 'a {
        self.turn_guard(turn, |t| t.sending = true, |t| t.sending = false)
    }

    /// Applies `on` to `turn` now and `off` when the guard drops.
    fn turn_guard<'a>(
        &'a self,
        turn: &'a DrawingTurn,
        on: impl FnOnce(&mut TurnState),
        off: impl FnOnce(&mut TurnState) + 'a,
    ) -> impl Drop + 'a {
        struct Guard<'a, F: FnOnce(&mut TurnState)> {
            cache: &'a FiguresCache,
            turn: &'a DrawingTurn,
            off: Option<F>,
        }
        impl<F: FnOnce(&mut TurnState)> Drop for Guard<'_, F> {
            fn drop(&mut self) {
                if let Some(off) = self.off.take() {
                    self.cache.turn(self.turn, off);
                }
            }
        }
        self.turn(turn, on);
        Guard {
            cache: self,
            turn,
            off: Some(off),
        }
    }

    /// Applies `change` to `turn` and moves the counts the builds read by what that changed.
    fn turn(&self, turn: &DrawingTurn, change: impl FnOnce(&mut TurnState)) {
        let mut state = turn.state.lock().unwrap_or_else(PoisonError::into_inner);
        let before = *state;
        change(&mut state);
        let after = *state;
        let shift = |count: &mut usize, was: bool, is: bool| match (was, is) {
            (false, true) => *count += 1,
            (true, false) => *count -= 1,
            _ => {}
        };
        let mut drawing = self.drawing.lock().unwrap_or_else(PoisonError::into_inner);
        shift(&mut drawing.draw, before.draws(), after.draws());
        shift(
            &mut drawing.wait_to_draw,
            before.waits_to_draw(),
            after.waits_to_draw(),
        );
        self.drawn.notify_all();
    }

    /// Adds a caller with `cancel` to `key`'s interest, until the guard drops.
    fn register<'a>(
        &'a self,
        key: &FiguresKey,
        cancel: Option<&crate::CancelToken>,
    ) -> (Arc<Interest>, impl Drop + 'a) {
        struct Caller<'a> {
            cache: &'a FiguresCache,
            key: FiguresKey,
            interest: Arc<Interest>,
            id: u64,
        }
        impl Drop for Caller<'_> {
            fn drop(&mut self) {
                let mut map = self
                    .cache
                    .interest
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut callers = self
                    .interest
                    .callers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                callers.retain(|(id, _)| *id != self.id);
                if callers.is_empty() {
                    map.remove(&self.key);
                }
            }
        }
        let id = self.next_caller.fetch_add(1, Ordering::Relaxed);
        let interest = {
            let mut map = self.interest.lock().unwrap_or_else(PoisonError::into_inner);
            let interest = Arc::clone(map.entry(key.clone()).or_default());
            interest
                .callers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, cancel.cloned()));
            interest
        };
        let caller = Caller {
            cache: self,
            key: key.clone(),
            interest: Arc::clone(&interest),
            id,
        };
        (interest, caller)
    }

    /// One of the [`CONCURRENT_BUILDS`] places to walk, held until the guard drops. `None` once
    /// every caller that wanted the build has gone.
    fn build_permit(&self, interest: &Interest) -> Option<impl Drop + '_> {
        struct Permit<'a>(&'a FiguresCache);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                *self
                    .0
                    .building
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) -= 1;
                self.0.built.notify_one();
            }
        }
        let mut building = self.building.lock().unwrap_or_else(PoisonError::into_inner);
        while *building >= CONCURRENT_BUILDS {
            if interest.abandoned() {
                return None;
            }
            building = self
                .built
                .wait_timeout(building, ABANDON_TICK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *building += 1;
        Some(Permit(self))
    }
}

impl Drop for FiguresCache {
    /// The writes queued, then the newest counts of every entry whose last write is older: a write
    /// is held back while an entry was written recently, and a clean shutdown keeps what that held
    /// back.
    fn drop(&mut self) {
        self.worker.finish();
        for (key, held, newest) in self.unwritten() {
            let Some(written) = &held.written else {
                continue;
            };
            let reserve = match &key {
                FiguresKey::Fragment(key) => self.reserve(key, newest.dense.filled_at),
                FiguresKey::Exact(_) | FiguresKey::Field(_) | FiguresKey::FieldLeft(_) => None,
            };
            super::persist::write_counts(&written.dir, &written.stem, &newest, reserve.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(terms: u8, layer: &str, overlay: u64) -> FiguresKey {
        FiguresKey::Exact(ExactKey {
            geometry: Geometry::None,
            terms: [terms; 32],
            view: "s0".into(),
            layer: layer.into(),
            level: 0,
            level_version: 1,
            segments_version: 1,
            projection_segments_version: 1,
            overlay_version: overlay,
            fragment_identity: [7u8; 32],
            fragment_watermark: 0,
        })
    }

    fn counts(values: &[u32]) -> FragmentCounts {
        FragmentCounts::new(CountsAt::of_counts(1, values.to_vec()), None)
    }

    fn get(
        cache: &FiguresCache,
        key: FiguresKey,
        build: impl FnOnce() -> FragmentCounts,
    ) -> Arc<FragmentCounts> {
        cache
            .get_or_build(key, &DrawingTurn::default(), None, |_| Some(build()))
            .expect("nothing else is building")
    }

    /// A hit does not rebuild, and a miss does.
    #[test]
    fn one_walk_per_key() {
        let cache = FiguresCache::default();
        let built = std::sync::atomic::AtomicU32::new(0);
        for _ in 0..3 {
            let held = get(&cache, key(1, "a", 0), || {
                built.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                counts(&[5, 6])
            });
            assert_eq!(held.newest().count(0), 5);
            assert_eq!(held.newest().count(1), 6);
            assert_eq!(
                held.newest().count(9),
                0,
                "past the level is zero, not a panic"
            );
        }
        assert_eq!(built.into_inner(), 1);
        assert_eq!(cache.stats().hits, 2);
        assert_eq!(cache.stats().misses, 1);
    }

    /// Requests that arrive while a key is building wait for that build rather than walking the
    /// mask again, and are handed what it built.
    #[test]
    fn concurrent_requests_for_one_key_share_one_build() {
        let cache = Arc::new(FiguresCache::default());
        let builds = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(std::sync::Mutex::new(held));
        let askers: Vec<_> = (0..4)
            .map(|_| {
                let (cache, builds, held) =
                    (Arc::clone(&cache), Arc::clone(&builds), Arc::clone(&held));
                std::thread::spawn(move || {
                    get(&cache, key(1, "a", 0), || {
                        builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        held.lock().unwrap().recv().unwrap();
                        counts(&[3, 4])
                    })
                    .newest()
                    .count(1)
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while cache.stats().waiters < 3 {
            assert!(
                std::time::Instant::now() < deadline,
                "the other requests never waited"
            );
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        for asker in askers {
            assert_eq!(asker.join().unwrap(), 4);
        }
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 3);
    }

    /// A waiter whose request goes away stops waiting, and the build it was waiting for is
    /// unaffected.
    #[test]
    fn a_waiter_leaves_when_its_request_does() {
        let cache = Arc::new(FiguresCache::default());
        let (release, held) = std::sync::mpsc::channel::<()>();
        let builder = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                get(&cache, key(1, "a", 0), || {
                    held.recv().unwrap();
                    counts(&[7])
                })
                .newest()
                .count(0)
            })
        };
        while cache.stats().misses == 0 {
            std::thread::yield_now();
        }
        let gone = crate::CancelToken::new();
        gone.cancel();
        let waited =
            cache.get_or_build(key(1, "a", 0), &DrawingTurn::default(), Some(&gone), |_| {
                Some(counts(&[0]))
            });
        assert_eq!(waited.err(), Some(WaitEnded::Cancelled));
        release.send(()).unwrap();
        assert_eq!(builder.join().unwrap(), 7);
    }

    /// No more than [`CONCURRENT_BUILDS`] builds walk at once, whatever their keys: the rest have
    /// claimed their keys and wait for a place.
    #[test]
    fn builds_beyond_the_limit_queue() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let cache = Arc::new(FiguresCache::default());
        let walking = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicBool::new(false));
        let builders: Vec<_> = (0..6u8)
            .map(|terms| {
                let (cache, walking, most, open) = (
                    Arc::clone(&cache),
                    Arc::clone(&walking),
                    Arc::clone(&most),
                    Arc::clone(&open),
                );
                std::thread::spawn(move || {
                    get(&cache, key(terms, "a", 0), || {
                        most.fetch_max(
                            walking.fetch_add(1, Ordering::SeqCst) + 1,
                            Ordering::SeqCst,
                        );
                        while !open.load(Ordering::SeqCst) {
                            std::thread::yield_now();
                        }
                        walking.fetch_sub(1, Ordering::SeqCst);
                        counts(&[u32::from(terms)])
                    })
                    .newest()
                    .count(0)
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while cache.stats().misses < 6 || walking.load(Ordering::SeqCst) < CONCURRENT_BUILDS {
            assert!(
                std::time::Instant::now() < deadline,
                "the builds never started"
            );
            std::thread::yield_now();
        }
        open.store(true, Ordering::SeqCst);
        for (terms, builder) in builders.into_iter().enumerate() {
            assert_eq!(builder.join().unwrap(), terms as u64);
        }
        assert_eq!(most.load(Ordering::SeqCst), CONCURRENT_BUILDS);
    }

    /// A deny moves the overlay's counter, so the key a request produces after the acknowledgement
    /// is not the key the pre-deny entry sits under. The pre-deny entry is not edited.
    #[test]
    fn a_deny_rotates_the_key_rather_than_editing_the_entry() {
        let cache = FiguresCache::default();
        let before = get(&cache, key(1, "a", 4), || counts(&[10]));
        assert_eq!(before.newest().count(0), 10);
        let after = get(&cache, key(1, "a", 5), || counts(&[9]));
        assert_eq!(
            after.newest().count(0),
            9,
            "the corrected count, not the held one"
        );
        assert_eq!(
            get(&cache, key(1, "a", 4), || counts(&[0]))
                .newest()
                .count(0),
            10
        );
    }

    /// Two term sets do not share counts, whatever else their keys have in common.
    #[test]
    fn the_counts_are_the_term_sets_own() {
        let cache = FiguresCache::default();
        assert_eq!(
            get(&cache, key(1, "a", 0), || counts(&[3]))
                .newest()
                .count(0),
            3
        );
        assert_eq!(
            get(&cache, key(2, "a", 0), || counts(&[8]))
                .newest()
                .count(0),
            8
        );
        assert_eq!(
            get(&cache, key(1, "a", 0), || counts(&[0]))
                .newest()
                .count(0),
            3
        );
    }

    /// The bound evicts the least recently used, lowering it reclaims at once, and an entry
    /// larger than the whole bound is served and not admitted.
    #[test]
    fn the_budget_bounds_what_is_resident() {
        let floor = mosaica_cache::PER_ENTRY_FLOOR_BYTES;
        let cache = FiguresCache::new(2 * floor, None);
        get(&cache, key(1, "a", 0), || counts(&[1, 1]));
        get(&cache, key(2, "a", 0), || counts(&[2, 2]));
        assert_eq!(cache.stats().entries, 2);

        get(&cache, key(1, "a", 0), || counts(&[0, 0]));
        get(&cache, key(3, "a", 0), || counts(&[3, 3]));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(
            get(&cache, key(1, "a", 0), || counts(&[0, 0]))
                .newest()
                .count(0),
            1
        );

        cache.set_bound_bytes(0);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().resident_bytes, 0);
        let held = get(&cache, key(4, "a", 0), || counts(&[9, 9]));
        assert_eq!(held.newest().count(0), 9);
        assert_eq!(cache.stats().entries, 0);
    }

    /// A build's walk waits while a request draws points, and goes on when it stops.
    #[test]
    fn a_build_gives_way_while_a_request_draws() {
        let cache = Arc::new(FiguresCache::default());
        cache.set_give_way_ms(600_000);
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started, walking) = std::sync::mpsc::channel::<()>();
        let turn = DrawingTurn::default();
        let drawing = cache.drawing(&turn);
        let builder = {
            let (cache, order) = (Arc::clone(&cache), Arc::clone(&order));
            std::thread::spawn(move || {
                cache
                    .get_or_build(key(1, "a", 0), &DrawingTurn::default(), None, |build| {
                        started.send(()).unwrap();
                        assert!(build.give_way());
                        order.lock().unwrap().push("walked");
                        Some(counts(&[1]))
                    })
                    .unwrap()
                    .newest()
                    .count(0)
            })
        };
        walking.recv().unwrap();
        order.lock().unwrap().push("drawn");
        drop(drawing);
        assert_eq!(builder.join().unwrap(), 1);
        assert_eq!(*order.lock().unwrap(), ["drawn", "walked"]);
    }

    /// A drawing request that builds counts, or waits on another's build of them, is not counted
    /// as drawing meanwhile, so neither build waits on it.
    #[test]
    fn a_request_waiting_on_a_build_is_not_counted_as_drawing() {
        let cache = Arc::new(FiguresCache::default());
        cache.set_give_way_ms(600_000);
        let turn = DrawingTurn::default();
        let _drawing = cache.drawing(&turn);

        let own = cache
            .get_or_build(key(1, "a", 0), &turn, None, |build| {
                assert!(build.give_way());
                Some(counts(&[3]))
            })
            .unwrap();
        assert_eq!(own.newest().count(0), 3);

        let (started, walking) = std::sync::mpsc::channel::<()>();
        let builder = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                cache
                    .get_or_build(key(2, "a", 0), &DrawingTurn::default(), None, |build| {
                        started.send(()).unwrap();
                        assert!(build.give_way());
                        Some(counts(&[5]))
                    })
                    .unwrap()
                    .newest()
                    .count(0)
            })
        };
        walking.recv().unwrap();
        let waited = cache
            .get_or_build(key(2, "a", 0), &turn, None, |_| Some(counts(&[0])))
            .unwrap();
        assert_eq!(waited.newest().count(0), 5);
        assert_eq!(builder.join().unwrap(), 5);
    }

    /// While a drawing request waits on a build, no build gives way to the other drawing requests:
    /// that request's points wait on the builds.
    #[test]
    fn no_build_gives_way_while_a_drawing_request_waits_on_one() {
        let cache = Arc::new(FiguresCache::default());
        cache.set_give_way_ms(600_000);
        let other = DrawingTurn::default();
        let _other_drawing = cache.drawing(&other);

        let (held_tx, held) = std::sync::mpsc::channel::<()>();
        let (started, building) = std::sync::mpsc::channel::<()>();
        let slow = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                get(&cache, key(1, "a", 0), || {
                    started.send(()).unwrap();
                    held.recv().unwrap();
                    counts(&[1])
                })
                .newest()
                .count(0)
            })
        };
        building.recv().unwrap();
        let waiting = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                let turn = DrawingTurn::default();
                let _drawing = cache.drawing(&turn);
                cache
                    .get_or_build(key(1, "a", 0), &turn, None, |_| Some(counts(&[0])))
                    .unwrap()
                    .newest()
                    .count(0)
            })
        };
        while cache.stats().waiters == 0 {
            std::thread::yield_now();
        }

        let (done, finished) = std::sync::mpsc::channel();
        let walker = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                let got = cache
                    .get_or_build(key(2, "a", 0), &DrawingTurn::default(), None, |build| {
                        assert!(build.give_way());
                        Some(counts(&[2]))
                    })
                    .unwrap()
                    .newest()
                    .count(0);
                done.send(got).unwrap();
            })
        };
        let walked = finished.recv_timeout(std::time::Duration::from_secs(60));
        held_tx.send(()).unwrap();
        assert_eq!(
            walked,
            Ok(2),
            "the walk gave way while a drawing request waited on a build"
        );
        walker.join().unwrap();
        assert_eq!(slow.join().unwrap(), 1);
        assert_eq!(waiting.join().unwrap(), 1);
    }

    /// A build waiting for a place stops once every caller that wanted it has gone, and holds
    /// nothing.
    #[test]
    fn a_build_waiting_for_a_place_stops_when_its_callers_go() {
        let cache = Arc::new(FiguresCache::default());
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        let (started, building) = std::sync::mpsc::channel::<()>();
        let walkers: Vec<_> = (0..CONCURRENT_BUILDS as u8)
            .map(|terms| {
                let (cache, held, started) =
                    (Arc::clone(&cache), Arc::clone(&held), started.clone());
                std::thread::spawn(move || {
                    get(&cache, key(terms, "a", 0), || {
                        started.send(()).unwrap();
                        held.lock().unwrap().recv().unwrap();
                        counts(&[u32::from(terms)])
                    })
                    .newest()
                    .count(0)
                })
            })
            .collect();
        for _ in 0..CONCURRENT_BUILDS {
            building.recv().unwrap();
        }

        let gone = crate::CancelToken::new();
        let queued = {
            let (cache, gone) = (Arc::clone(&cache), gone.clone());
            std::thread::spawn(move || {
                cache.get_or_build(key(9, "a", 0), &DrawingTurn::default(), Some(&gone), |_| {
                    Some(counts(&[9]))
                })
            })
        };
        while cache.stats().misses <= CONCURRENT_BUILDS as u64 {
            std::thread::yield_now();
        }
        gone.cancel();
        assert_eq!(queued.join().unwrap().err(), Some(WaitEnded::Cancelled));
        for _ in 0..CONCURRENT_BUILDS {
            release.send(()).unwrap();
        }
        for walker in walkers {
            walker.join().unwrap();
        }
        assert_eq!(
            cache.stats().entries,
            CONCURRENT_BUILDS,
            "the stopped build holds nothing"
        );
    }
}
