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
//! - `overlay_version`: moved by every overlay or buffer publication, which is every accepted
//!   deletion, suppression, lift and ingest. A request loads its generation once, at its start, so
//!   a request that starts after a suppression is accepted reads a key no earlier entry or build
//!   holds. An unsuppress moves it again and the entry is built afresh, which is why
//!   `delete, suppress, unsuppress` leaves the entity deleted here too.
//! - `fragment_identity` and `fragment_watermark`: the fragment the session's projection was built
//!   from. A session may be served a projection one generation stale, so two requests at one
//!   `segments_version` can compose against different fragments. A fold rotates the identity.
//!
//! The filter is not a term: the count beside an artifact does not depend on a filter, so a
//! filtered request and an unfiltered one read the same entry.
//!
//! # Single flight, removal only, and a byte bound
//!
//! Concurrent requests for one key share one build ([`tessera_cache::SingleFlightCache`]). Nothing
//! mutates a held value: eviction only removes, and a rebuild walks the same mask over the same
//! column, so the bound decides what is resident and never what is served. An entry larger than
//! the whole bound is handed to its caller and not admitted.
//!
//! An entry is 4 B an artifact for counts alone and 40 B with the geometry: a count, a placed
//! count, two `u64` sums and four `u32` bounds. At 1.4×10⁶ artifacts that is 56 MB a level. The
//! bound is `serve.masked_count_cache_bytes`, 256 MiB unless configured.

use std::sync::Arc;

use tessera_cache::{CacheWeight, NeverCancelled, SingleFlightCache};

use crate::row_column::LevelAccumulation;

/// How long a request waits for another request's build of the same key before building its own.
/// Far beyond any build measured, so a second request on a key never repeats the walk.
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

/// The cache itself, under a byte bound.
pub struct MaskedCountCache {
    slots: SingleFlightCache<MaskedCountKey, MaskedCounts>,
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
        MaskedCountCache { slots }
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
    /// builds the same key waits for that build and is handed its result.
    ///
    /// `build` is called at most once per call. It is called here, uncached, only if a wait
    /// outlasts [`BUILD_WAIT_MS`].
    pub(crate) fn get_or_build(
        &self,
        key: MaskedCountKey,
        build: impl Fn() -> MaskedCounts,
    ) -> Arc<MaskedCounts> {
        self.slots
            .get_or_derive_waiting(key, None, &NeverCancelled, |_| build())
            .unwrap_or_else(|_| Arc::new(build()))
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
            overlay_version: overlay,
            fragment_identity: [7u8; 32],
            fragment_watermark: 0,
        }
    }

    fn counts(values: &[u32]) -> MaskedCounts {
        MaskedCounts::new(values.to_vec())
    }

    /// A hit does not rebuild, and a miss does.
    #[test]
    fn one_walk_per_key() {
        let cache = MaskedCountCache::default();
        let built = std::sync::atomic::AtomicU32::new(0);
        for _ in 0..3 {
            let held = cache.get_or_build(key(1, "a", 0), || {
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
                    cache
                        .get_or_build(key(1, "a", 0), || {
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

    /// A deny moves the overlay's counter, so the key a request produces after the acknowledgement
    /// is not the key the pre-deny entry sits under. The pre-deny entry is not edited.
    #[test]
    fn a_deny_rotates_the_key_rather_than_editing_the_entry() {
        let cache = MaskedCountCache::default();
        let before = cache.get_or_build(key(1, "a", 4), || counts(&[10]));
        assert_eq!(before.get(0), 10);
        let after = cache.get_or_build(key(1, "a", 5), || counts(&[9]));
        assert_eq!(after.get(0), 9, "the corrected count, not the held one");
        assert_eq!(
            cache.get_or_build(key(1, "a", 4), || counts(&[0])).get(0),
            10
        );
    }

    /// Two term sets do not share counts, whatever else their keys have in common.
    #[test]
    fn the_counts_are_the_term_sets_own() {
        let cache = MaskedCountCache::default();
        assert_eq!(cache.get_or_build(key(1, "a", 0), || counts(&[3])).get(0), 3);
        assert_eq!(cache.get_or_build(key(2, "a", 0), || counts(&[8])).get(0), 8);
        assert_eq!(cache.get_or_build(key(1, "a", 0), || counts(&[0])).get(0), 3);
    }

    /// The bound evicts the least recently used, lowering it reclaims at once, and an entry
    /// larger than the whole bound is served and not admitted.
    #[test]
    fn the_budget_bounds_what_is_resident() {
        let floor = tessera_cache::PER_ENTRY_FLOOR_BYTES;
        let cache = MaskedCountCache::new(2 * floor);
        cache.get_or_build(key(1, "a", 0), || counts(&[1, 1]));
        cache.get_or_build(key(2, "a", 0), || counts(&[2, 2]));
        assert_eq!(cache.stats().entries, 2);

        cache.get_or_build(key(1, "a", 0), || counts(&[0, 0]));
        cache.get_or_build(key(3, "a", 0), || counts(&[3, 3]));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.get_or_build(key(1, "a", 0), || counts(&[0, 0])).get(0), 1);

        cache.set_bound_bytes(0);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().resident_bytes, 0);
        let held = cache.get_or_build(key(4, "a", 0), || counts(&[9, 9]));
        assert_eq!(held.get(0), 9);
        assert_eq!(cache.stats().entries, 0);
    }
}
