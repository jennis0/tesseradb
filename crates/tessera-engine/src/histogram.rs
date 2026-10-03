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
//! any viewport is drawing points ([`MaskedCountCache::drawing`]), from the start of its sweep to
//! its last point. A request is not counted while it waits on a build, its own included, so
//! nothing waits on itself. A drawing request's sends to its client count, so a client that stalls
//! holds the builds until the stream's stall bound sheds it.
//!
//! An entry is 4 B an artifact for counts alone and 40 B with the geometry: a count, a placed
//! count, two `u64` sums and four `u32` bounds. At 1.4×10⁶ artifacts that is 56 MB a level. The
//! bound is `serve.masked_count_cache_bytes`, 256 MiB unless configured.

use std::sync::Arc;

use std::sync::{Condvar, Mutex, PoisonError};

use tessera_cache::{Cancel, CacheWeight, SingleFlightCache, WaitEnded};

use crate::row_column::LevelAccumulation;

/// How many builds may walk at once.
const CONCURRENT_BUILDS: usize = 2;

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

/// One request's part in [`MaskedCountCache::drawing`]: whether it is drawing points, and how
/// many of its calls are waiting on a build. It counts as drawing only while it is and none is.
#[derive(Debug, Default)]
pub(crate) struct DrawingTurn(Mutex<(bool, usize)>);

/// The cache itself, under a byte bound.
pub struct MaskedCountCache {
    slots: SingleFlightCache<MaskedCountKey, MaskedCounts>,
    /// Builds walking now, at most [`CONCURRENT_BUILDS`].
    building: Mutex<usize>,
    built: Condvar,
    /// Requests drawing points now. A build walks only while this is zero.
    drawing: Mutex<usize>,
    drawn: Condvar,
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
            drawing: Mutex::new(0),
            drawn: Condvar::new(),
        }
    }

    /// Move the ceiling, and drop what is held so the memory comes back at once rather than at
    /// the next build.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.slots.set_bound_bytes(bound_bytes);
        self.slots.retain_keys(|_| false);
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
    /// for the duration.
    pub(crate) fn get_or_build<C: Cancel + ?Sized>(
        &self,
        key: MaskedCountKey,
        turn: &DrawingTurn,
        cancel: &C,
        build: impl FnOnce() -> MaskedCounts,
    ) -> Result<Arc<MaskedCounts>, WaitEnded> {
        self.turn(turn, |t| t.1 += 1);
        let got = self.slots.get_or_derive_waiting(key, None, cancel, |_| {
            let _permit = self.build_permit();
            build()
        });
        self.turn(turn, |t| t.1 -= 1);
        got
    }

    /// Counts `turn`'s request as drawing points until the guard drops.
    pub(crate) fn drawing<'a>(&'a self, turn: &'a DrawingTurn) -> impl Drop + 'a {
        struct Drawing<'a>(&'a MaskedCountCache, &'a DrawingTurn);
        impl Drop for Drawing<'_> {
            fn drop(&mut self) {
                self.0.turn(self.1, |t| t.0 = false);
            }
        }
        self.turn(turn, |t| t.0 = true);
        Drawing(self, turn)
    }

    /// Returns once no request is drawing points. A build calls it between chunks of its walk.
    pub(crate) fn give_way(&self) {
        let mut drawing = self.drawing.lock().unwrap_or_else(PoisonError::into_inner);
        while *drawing > 0 {
            drawing = self.drawn.wait(drawing).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Applies `change` to `turn` and moves the drawing count by what that changed.
    fn turn(&self, turn: &DrawingTurn, change: impl FnOnce(&mut (bool, usize))) {
        let counts = |t: &(bool, usize)| t.0 && t.1 == 0;
        let mut t = turn.0.lock().unwrap_or_else(PoisonError::into_inner);
        let before = counts(&t);
        change(&mut t);
        let after = counts(&t);
        if before == after {
            return;
        }
        let mut drawing = self.drawing.lock().unwrap_or_else(PoisonError::into_inner);
        if after {
            *drawing += 1;
        } else {
            *drawing -= 1;
            if *drawing == 0 {
                self.drawn.notify_all();
            }
        }
    }

    /// One of the [`CONCURRENT_BUILDS`] places to walk, held until the guard drops.
    fn build_permit(&self) -> impl Drop + '_ {
        struct Permit<'a>(&'a MaskedCountCache);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                *self.0.building.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
                self.0.built.notify_one();
            }
        }
        let mut building = self.building.lock().unwrap_or_else(PoisonError::into_inner);
        while *building >= CONCURRENT_BUILDS {
            building = self.built.wait(building).unwrap_or_else(PoisonError::into_inner);
        }
        *building += 1;
        Permit(self)
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
            .get_or_build(key, &DrawingTurn::default(), &tessera_cache::NeverCancelled, build)
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
        let waited = cache.get_or_build(key(1, "a", 0), &DrawingTurn::default(), &gone, || counts(&[0]));
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
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started, walking) = std::sync::mpsc::channel::<()>();
        let turn = DrawingTurn::default();
        let drawing = cache.drawing(&turn);
        let builder = {
            let (cache, order) = (Arc::clone(&cache), Arc::clone(&order));
            std::thread::spawn(move || {
                get(&cache, key(1, "a", 0), || {
                    started.send(()).unwrap();
                    cache.give_way();
                    order.lock().unwrap().push("walked");
                    counts(&[1])
                })
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
        let turn = DrawingTurn::default();
        let _drawing = cache.drawing(&turn);

        let own = cache
            .get_or_build(key(1, "a", 0), &turn, &tessera_cache::NeverCancelled, || {
                cache.give_way();
                counts(&[3])
            })
            .unwrap();
        assert_eq!(own.get(0), 3);

        let (started, walking) = std::sync::mpsc::channel::<()>();
        let builder = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                get(&cache, key(2, "a", 0), || {
                    started.send(()).unwrap();
                    cache.give_way();
                    counts(&[5])
                })
                .get(0)
            })
        };
        walking.recv().unwrap();
        let waited = cache
            .get_or_build(key(2, "a", 0), &turn, &tessera_cache::NeverCancelled, || counts(&[0]))
            .unwrap();
        assert_eq!(waited.get(0), 5);
        assert_eq!(builder.join().unwrap(), 5);
    }
}
