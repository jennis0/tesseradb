//! The masked counts of a level served from its column: per artifact, how many rows of the
//! composed mask carry its label, and where the level's geometry is accumulated, the position sum
//! and bounding box of those rows.
//!
//! A level that holds one bitmap per artifact counts one artifact at a time, so a request's
//! budget bounds the work. A level served from its column has no per-artifact membership, and its
//! only route to a count is a walk of every visible row ([`crate::row_column::RowColumn::accumulate`]).
//! On a large view that walk takes seconds, so its result is held.
//!
//! # What an entry is shared by
//!
//! An entry is a function of the composed mask and of the level's column, and the key names every
//! input to each. The composed mask is a function of the session's satisfied term set, the
//! fragment its projection was built from, and the generation's segments, overlay and buffer
//! ([`crate::compose::compose`]); nothing else about a session reaches it. So the key carries a
//! digest of the term set in place of the session, and every session holding the same term set at
//! the same coordinates reads one entry and shares one build.
//!
//! - `terms`: SHA-256 over the sorted satisfied terms, four bytes each.
//! - `view`, `layer`, `level`: what the column is of.
//! - `level_version`: the version of the row form the walk read, which is not always the store's.
//!   A form waiting for a tick's delta stands at the earlier version, and it comes back from
//!   `ArtifactProjections::get_or_build` beside the form so the two cannot disagree.
//! - `geometry`: whether the entry carries the position sums and boxes. A browse page wants the
//!   counts alone and a viewport deriving a centroid wants both, so each pays for what it reads.
//! - `segments_version`: row ids mean something only within one geometry. A flush and a fold
//!   both move it.
//! - `projection_segments_version`: the generation the session's row projection was built at.
//!   A session may be served a projection one generation stale, which the rest of the key would
//!   not otherwise tell from a fresh one.
//! - `overlay_version`: moved by every overlay or buffer publication, which is every accepted
//!   deletion, suppression, lift and ingest. A request loads its generation once, at its start, so
//!   a request that starts after a suppression is accepted reads a key no earlier entry or build
//!   holds. An unsuppress moves it again and the entry is built afresh, which is why
//!   `delete, suppress, unsuppress` leaves the entity deleted here too.
//! - `fragment_identity` and `fragment_watermark`: the fragment the session's projection was built
//!   from. A fold rotates the identity.
//!
//! The filter is not a term: the count beside an artifact does not depend on a filter, so a
//! filtered request and an unfiltered one read the same entry.
//!
//! # Single flight, removal only, and a byte bound
//!
//! Concurrent requests for one key share one build ([`tessera_cache::SingleFlightCache`]). A
//! request waits for another's build for as long as its client stays connected, up to
//! [`BUILD_WAIT_MS`]. A build on a large view takes longer than `serve.single_flight_wait_ms`,
//! and a viewport has sent its head before it reads the counts, so a refusal at that budget would
//! cut a response that the build was about to complete. At most
//! [`CONCURRENT_BUILDS`] builds walk at once, each holding an accumulator per worker of the count
//! pool, so live ingest moving `overlay_version` under many term sets queues their rebuilds
//! rather than holding every accumulator at once. Nothing
//! mutates a held value: eviction only removes, and a rebuild walks the same mask over the same
//! column, so the bound decides what is resident and never what is served. An entry larger than
//! the whole bound is handed to its caller and not admitted.
//!
//! # A build gives way to points
//!
//! A build walks every visible row of a level, which on a corpus larger than memory streams from
//! disk, and a viewport's reads queue behind it. So a build waits between chunks of its walk while
//! any viewport is drawing points ([`MaskedCountCache::drawing`]): from the start of its sweep to
//! its last point, less the time it is blocked handing a frame to its client. A build gives way
//! for at most `serve.masked_count_give_way_ms` from its first wait, and not at all while a
//! drawing request is itself waiting on a build, since that request's points wait on the builds.
//! A request is not counted as drawing while it waits on a build, its own included, so nothing
//! waits on itself. A build, or a build waiting for a place, that every caller has left stops
//! and holds nothing.
//!
//! An entry is 4 B an artifact for counts alone and 40 B with the geometry: a count, a placed
//! count, two `u64` sums and four `u32` bounds. At 1.4×10⁶ artifacts that is 56 MB a level. The
//! bound is `serve.masked_count_cache_bytes`, 256 MiB unless configured.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tessera_cache::{Cancel, CacheWeight, SingleFlightCache, WaitEnded, WaitingBuildError};

use crate::row_column::LevelAccumulation;

/// How many builds may walk at once.
const CONCURRENT_BUILDS: usize = 2;

/// How long a build gives way to drawing requests, from its first wait, unless
/// `serve.masked_count_give_way_ms` says otherwise.
pub const DEFAULT_GIVE_WAY_MS: u64 = 2_000;

/// How long a connected request waits for another's build before it is refused. Far beyond any
/// build measured, so it ends a wait only on a build that has stopped making progress.
const BUILD_WAIT_MS: u64 = 600_000;

/// What one entry is a function of. See the module doc for each term.
///
/// Named fields rather than a tuple: five of the terms are integers of one type, and a
/// transposition at the construction site would compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MaskedCountKey {
    pub terms: [u8; 32],
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub level_version: u64,
    pub geometry: bool,
    pub segments_version: u64,
    pub projection_segments_version: u64,
    pub overlay_version: u64,
    pub fragment_identity: [u8; 32],
    pub fragment_watermark: u64,
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
    /// This mask's key for one level.
    pub(crate) fn key(
        &self,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        geometry: bool,
    ) -> MaskedCountKey {
        MaskedCountKey {
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

/// One level's masked counts, by ordinal.
#[derive(Debug)]
pub struct MaskedCounts {
    counts: Vec<u32>,
    /// The accumulated geometry, where the level is served from its column alone. `None` on every
    /// other level, whose derived content is computed from each artifact's own row bitmap.
    geometry: Option<MaskedGeometry>,
}

/// Per ordinal, the position sum and bounding box of the members this viewer may see: the inputs a
/// centroid and a box are functions of, accumulated in the pass that counts them. The pass reads
/// only rows the composed mask admits.
#[derive(Debug)]
pub struct MaskedGeometry {
    /// How many of the counted rows the row space could place — the divisor for the mean. Not the
    /// masked count, which counts every visible row.
    placed: Vec<u32>,
    /// Exact: see [`LevelAccumulation::sums`].
    sums: Vec<[u64; 2]>,
    boxes: Vec<[u32; 4]>,
}

impl MaskedGeometry {
    /// The mean position of the members this viewer may see, or `None` where they see none.
    pub fn centroid(&self, ordinal: u32) -> Option<[f64; 2]> {
        let i = ordinal as usize;
        let n = *self.placed.get(i)? as f64;
        if n == 0.0 {
            return None;
        }
        let s = self.sums.get(i)?;
        Some([s[0] as f64 / n, s[1] as f64 / n])
    }

    /// `[x_min, y_min, x_max, y_max]` over the members this viewer may see, or `None` where they
    /// see none.
    pub fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        let i = ordinal as usize;
        if *self.placed.get(i)? == 0 {
            return None;
        }
        self.boxes.get(i).copied()
    }

    fn weight_bytes(&self) -> u64 {
        (self.placed.len() * (std::mem::size_of::<u32>() + 16 + 16)) as u64
    }
}

impl MaskedCounts {
    #[cfg(test)]
    pub(crate) fn new(counts: Vec<u32>) -> Self {
        MaskedCounts {
            counts,
            geometry: None,
        }
    }

    /// What one pass folded up, with the geometry where the pass was given positions.
    pub(crate) fn of(accumulation: LevelAccumulation, geometry: bool) -> Self {
        let LevelAccumulation {
            counts,
            placed,
            sums,
            boxes,
        } = accumulation;
        MaskedCounts {
            counts,
            geometry: geometry.then_some(MaskedGeometry {
                placed,
                sums,
                boxes,
            }),
        }
    }

    /// See [`Self::geometry`].
    pub fn geometry(&self) -> Option<&MaskedGeometry> {
        self.geometry.as_ref()
    }

    /// `|membership ∩ M_auth|` for one artifact.
    ///
    /// **Zero past the end**, which is a hole or an ordinal the column does not cover — the same
    /// answer the row form gives for a slot with no membership, and the fail-closed one: an
    /// artifact counted at zero is absent under any criterion and carries a zero beside it under
    /// none.
    pub fn get(&self, ordinal: u32) -> u64 {
        self.counts
            .get(ordinal as usize)
            .copied()
            .map(u64::from)
            .unwrap_or(0)
    }

    /// Every ordinal whose count is non-zero, ascending.
    ///
    /// This is candidacy for a row-major level at a viewport covering the whole mask.
    /// [`crate::artifacts::ArtifactRows::candidacy`] carries the argument for why the two are the
    /// same set.
    ///
    /// Collected and added in one call rather than one `add` per ordinal. At the rung 6 corpus's
    /// 1.65×10⁶ ordinals the per-ordinal form is 1.65×10⁶ crossings of the bitmap library's
    /// boundary, for a set the library can build from a sorted slice in one.
    pub fn populated(&self) -> croaring::Bitmap {
        let hits: Vec<u32> = self
            .counts
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(ordinal, _)| ordinal as u32)
            .collect();
        let mut out = croaring::Bitmap::new();
        out.add_many(&hits);
        out.run_optimize();
        out
    }

    /// How many ordinals this covers.
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    fn weight_bytes(&self) -> u64 {
        (self.counts.len() * std::mem::size_of::<u32>()) as u64
            + self.geometry.as_ref().map_or(0, MaskedGeometry::weight_bytes)
    }
}

impl CacheWeight for MaskedCounts {
    fn cache_weight_bytes(&self) -> u64 {
        self.weight_bytes()
    }
}

/// The gauges an operator reads. Names no artifact and no principal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaskedCountStats {
    /// Requests answered from a held entry, or from another request's build of it.
    pub hits: u64,
    /// Builds started.
    pub misses: u64,
    pub evictions: u64,
    /// Bytes currently held.
    pub resident_bytes: u64,
    /// Entries currently held, and builds in flight.
    pub entries: usize,
    /// Requests waiting at this instant for another request's build of their key.
    pub waiters: u64,
}

/// One request's part in [`MaskedCountCache::drawing`].
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
        callers
            .iter()
            .all(|(_, cancel)| cancel.as_ref().is_some_and(crate::CancelToken::is_cancelled))
    }
}

/// How often a build parked for a place, or giving way, looks at whether anyone still wants it.
const ABANDON_TICK: Duration = Duration::from_millis(20);

/// One build in flight, handed to the walk.
pub(crate) struct Build<'a> {
    cache: &'a MaskedCountCache,
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
        let mut drawing = self.cache.drawing.lock().unwrap_or_else(PoisonError::into_inner);
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
pub struct MaskedCountCache {
    slots: SingleFlightCache<MaskedCountKey, MaskedCounts>,
    /// Builds walking now, at most [`CONCURRENT_BUILDS`].
    building: Mutex<usize>,
    built: Condvar,
    drawing: Mutex<Drawing>,
    drawn: Condvar,
    /// How long a build gives way, from its first wait (`serve.masked_count_give_way_ms`).
    give_way_ms: AtomicU64,
    /// Who wants each key's build.
    interest: Mutex<FxHashMap<MaskedCountKey, Arc<Interest>>>,
    next_caller: AtomicU64,
}

impl Default for MaskedCountCache {
    fn default() -> Self {
        Self::new(u64::MAX)
    }
}

impl std::fmt::Debug for MaskedCountCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaskedCountCache")
            .field("stats", &self.stats())
            .finish()
    }
}

impl MaskedCountCache {
    /// `bound_bytes` is the resident-byte ceiling. `u64::MAX` means no bound, which every
    /// construction site outside the server has until [`Self::set_bound_bytes`] is called.
    pub fn new(bound_bytes: u64) -> Self {
        let slots = SingleFlightCache::new(bound_bytes);
        slots.set_wait_budget_ms(BUILD_WAIT_MS);
        MaskedCountCache {
            slots,
            building: Mutex::new(0),
            built: Condvar::new(),
            drawing: Mutex::new(Drawing::default()),
            drawn: Condvar::new(),
            give_way_ms: AtomicU64::new(DEFAULT_GIVE_WAY_MS),
            interest: Mutex::new(FxHashMap::default()),
            next_caller: AtomicU64::new(0),
        }
    }

    /// Move the ceiling, and drop what is held so the memory comes back at once rather than at
    /// the next build.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.slots.set_bound_bytes(bound_bytes);
        self.slots.retain_keys(|_| false);
    }

    /// How long a build gives way to drawing requests, from its first wait.
    pub fn set_give_way_ms(&self, give_way_ms: u64) {
        self.give_way_ms.store(give_way_ms, Ordering::Relaxed);
        self.drawn.notify_all();
    }

    pub fn stats(&self) -> MaskedCountStats {
        let stats = self.slots.stats();
        MaskedCountStats {
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            resident_bytes: stats.bytes,
            entries: stats.entries,
            waiters: stats.waiters_now,
        }
    }

    /// This key's counts, building them if nothing is held. A request arriving while another
    /// builds the same key waits for that build, while `cancel` holds and up to
    /// [`BUILD_WAIT_MS`], and is handed its result. `turn`'s request is not counted as drawing
    /// for the duration. A build that every caller has left stops, holds nothing, and its
    /// callers are answered [`WaitEnded::Cancelled`].
    pub(crate) fn get_or_build(
        &self,
        key: MaskedCountKey,
        turn: &DrawingTurn,
        cancel: Option<&crate::CancelToken>,
        walk: impl FnOnce(&Build<'_>) -> Option<MaskedCounts>,
    ) -> Result<Arc<MaskedCounts>, WaitEnded> {
        let _waiting = self.turn_guard(turn, |t| t.waiting += 1, |t| t.waiting -= 1);
        let (interest, _caller) = self.register(&key, cancel);
        let polled: &dyn Cancel = match cancel {
            Some(cancel) => cancel,
            None => &tessera_cache::NeverCancelled,
        };
        self.slots
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
            cache: &'a MaskedCountCache,
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
        key: &MaskedCountKey,
        cancel: Option<&crate::CancelToken>,
    ) -> (Arc<Interest>, impl Drop + 'a) {
        struct Caller<'a> {
            cache: &'a MaskedCountCache,
            key: MaskedCountKey,
            interest: Arc<Interest>,
            id: u64,
        }
        impl Drop for Caller<'_> {
            fn drop(&mut self) {
                let mut map = self.cache.interest.lock().unwrap_or_else(PoisonError::into_inner);
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
        struct Permit<'a>(&'a MaskedCountCache);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                *self.0.building.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(terms: u8, layer: &str, overlay: u64) -> MaskedCountKey {
        MaskedCountKey {
            geometry: false,
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
        }
    }

    fn counts(values: &[u32]) -> MaskedCounts {
        MaskedCounts::new(values.to_vec())
    }

    fn get(
        cache: &MaskedCountCache,
        key: MaskedCountKey,
        build: impl FnOnce() -> MaskedCounts,
    ) -> Arc<MaskedCounts> {
        cache
            .get_or_build(key, &DrawingTurn::default(), None, |_| Some(build()))
            .expect("nothing else is building")
    }

    /// A hit does not rebuild, and a miss does.
    #[test]
    fn one_walk_per_key() {
        let cache = MaskedCountCache::default();
        let built = std::sync::atomic::AtomicU32::new(0);
        for _ in 0..3 {
            let held = get(&cache, key(1, "a", 0), || {
                built.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                counts(&[5, 6])
            });
            assert_eq!(held.get(0), 5);
            assert_eq!(held.get(1), 6);
            assert_eq!(held.get(9), 0, "past the level is zero, not a panic");
        }
        assert_eq!(built.into_inner(), 1);
        assert_eq!(cache.stats().hits, 2);
        assert_eq!(cache.stats().misses, 1);
    }

    /// Requests that arrive while a key is building wait for that build rather than walking the
    /// mask again, and are handed what it built.
    #[test]
    fn concurrent_requests_for_one_key_share_one_build() {
        let cache = Arc::new(MaskedCountCache::default());
        let builds = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(std::sync::Mutex::new(held));
        let askers: Vec<_> = (0..4)
            .map(|_| {
                let (cache, builds, held) = (Arc::clone(&cache), Arc::clone(&builds), Arc::clone(&held));
                std::thread::spawn(move || {
                    get(&cache, key(1, "a", 0), || {
                        builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        held.lock().unwrap().recv().unwrap();
                        counts(&[3, 4])
                    })
                    .get(1)
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while cache.stats().waiters < 3 {
            assert!(std::time::Instant::now() < deadline, "the other requests never waited");
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
        let cache = Arc::new(MaskedCountCache::default());
        let (release, held) = std::sync::mpsc::channel::<()>();
        let builder = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                get(&cache, key(1, "a", 0), || {
                    held.recv().unwrap();
                    counts(&[7])
                })
                .get(0)
            })
        };
        while cache.stats().misses == 0 {
            std::thread::yield_now();
        }
        let gone = crate::CancelToken::new();
        gone.cancel();
        let waited = cache.get_or_build(key(1, "a", 0), &DrawingTurn::default(), Some(&gone), |_| {
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
        let cache = Arc::new(MaskedCountCache::default());
        let walking = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicBool::new(false));
        let builders: Vec<_> = (0..6u8)
            .map(|terms| {
                let (cache, walking, most, open) =
                    (Arc::clone(&cache), Arc::clone(&walking), Arc::clone(&most), Arc::clone(&open));
                std::thread::spawn(move || {
                    get(&cache, key(terms, "a", 0), || {
                        most.fetch_max(walking.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        while !open.load(Ordering::SeqCst) {
                            std::thread::yield_now();
                        }
                        walking.fetch_sub(1, Ordering::SeqCst);
                        counts(&[u32::from(terms)])
                    })
                    .get(0)
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while cache.stats().misses < 6 || walking.load(Ordering::SeqCst) < CONCURRENT_BUILDS {
            assert!(std::time::Instant::now() < deadline, "the builds never started");
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
        let cache = MaskedCountCache::default();
        let before = get(&cache, key(1, "a", 4), || counts(&[10]));
        assert_eq!(before.get(0), 10);
        let after = get(&cache, key(1, "a", 5), || counts(&[9]));
        assert_eq!(after.get(0), 9, "the corrected count, not the held one");
        assert_eq!(
            get(&cache, key(1, "a", 4), || counts(&[0])).get(0),
            10
        );
    }

    /// Two term sets do not share counts, whatever else their keys have in common.
    #[test]
    fn the_counts_are_the_term_sets_own() {
        let cache = MaskedCountCache::default();
        assert_eq!(get(&cache, key(1, "a", 0), || counts(&[3])).get(0), 3);
        assert_eq!(get(&cache, key(2, "a", 0), || counts(&[8])).get(0), 8);
        assert_eq!(get(&cache, key(1, "a", 0), || counts(&[0])).get(0), 3);
    }

    /// The bound evicts the least recently used, lowering it reclaims at once, and an entry
    /// larger than the whole bound is served and not admitted.
    #[test]
    fn the_budget_bounds_what_is_resident() {
        let floor = tessera_cache::PER_ENTRY_FLOOR_BYTES;
        let cache = MaskedCountCache::new(2 * floor);
        get(&cache, key(1, "a", 0), || counts(&[1, 1]));
        get(&cache, key(2, "a", 0), || counts(&[2, 2]));
        assert_eq!(cache.stats().entries, 2);

        get(&cache, key(1, "a", 0), || counts(&[0, 0]));
        get(&cache, key(3, "a", 0), || counts(&[3, 3]));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(get(&cache, key(1, "a", 0), || counts(&[0, 0])).get(0), 1);

        cache.set_bound_bytes(0);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().resident_bytes, 0);
        let held = get(&cache, key(4, "a", 0), || counts(&[9, 9]));
        assert_eq!(held.get(0), 9);
        assert_eq!(cache.stats().entries, 0);
    }

    /// A build's walk waits while a request draws points, and goes on when it stops.
    #[test]
    fn a_build_gives_way_while_a_request_draws() {
        let cache = Arc::new(MaskedCountCache::default());
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
                    .get(0)
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
        let cache = Arc::new(MaskedCountCache::default());
        cache.set_give_way_ms(600_000);
        let turn = DrawingTurn::default();
        let _drawing = cache.drawing(&turn);

        let own = cache
            .get_or_build(key(1, "a", 0), &turn, None, |build| {
                assert!(build.give_way());
                Some(counts(&[3]))
            })
            .unwrap();
        assert_eq!(own.get(0), 3);

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
                    .get(0)
            })
        };
        walking.recv().unwrap();
        let waited = cache
            .get_or_build(key(2, "a", 0), &turn, None, |_| Some(counts(&[0])))
            .unwrap();
        assert_eq!(waited.get(0), 5);
        assert_eq!(builder.join().unwrap(), 5);
    }

    /// While a drawing request waits on a build, no build gives way to the other drawing requests:
    /// that request's points wait on the builds.
    #[test]
    fn no_build_gives_way_while_a_drawing_request_waits_on_one() {
        let cache = Arc::new(MaskedCountCache::default());
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
                .get(0)
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
                    .get(0)
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
                    .get(0);
                done.send(got).unwrap();
            })
        };
        let walked = finished.recv_timeout(std::time::Duration::from_secs(60));
        held_tx.send(()).unwrap();
        assert_eq!(walked, Ok(2), "the walk gave way while a drawing request waited on a build");
        walker.join().unwrap();
        assert_eq!(slow.join().unwrap(), 1);
        assert_eq!(waiting.join().unwrap(), 1);
    }

    /// A build waiting for a place stops once every caller that wanted it has gone, and holds
    /// nothing.
    #[test]
    fn a_build_waiting_for_a_place_stops_when_its_callers_go() {
        let cache = Arc::new(MaskedCountCache::default());
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        let (started, building) = std::sync::mpsc::channel::<()>();
        let walkers: Vec<_> = (0..CONCURRENT_BUILDS as u8)
            .map(|terms| {
                let (cache, held, started) = (Arc::clone(&cache), Arc::clone(&held), started.clone());
                std::thread::spawn(move || {
                    get(&cache, key(terms, "a", 0), || {
                        started.send(()).unwrap();
                        held.lock().unwrap().recv().unwrap();
                        counts(&[u32::from(terms)])
                    })
                    .get(0)
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
        assert_eq!(cache.stats().entries, CONCURRENT_BUILDS, "the stopped build holds nothing");
    }
}
