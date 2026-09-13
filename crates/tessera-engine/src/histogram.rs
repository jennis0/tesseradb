//! The masked-count histogram cache — **the one structure sized by the artifact population that a
//! session is allowed to hold**, and the reasons it is allowed are narrow.
//!
//! [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
//! rules that nothing sized by the artifact population is held per token: there are a great many
//! tokens, principals do not share masks, and at 10⁷ artifacts a per-token structure is 0.85–1.7 s
//! of setup and ~40 MB of state per token per generation. It names **one exception**, and this is
//! it: a **row-major** layer may hold a masked-count histogram per `(session, layer)`.
//!
//! What makes this the one place the rule gives way is that a row-major level has **no other route**
//! to the quantity the disclosure rule requires. An artifact-major level answers
//! `|membership ∩ M_auth|` one artifact at a time, so a request's budget bounds the work; a
//! row-major level has no per-artifact membership to intersect, and its only route is a walk of the
//! mask reading off which artifact each visible row belongs to. Declining the exception would not
//! save that walk — it would pay it on every request instead of once a session.
//!
//! # The key is the whole of the safety, and one of its terms is a deny
//!
//! A histogram is a function of the composed mask and of the level's column, so every term below is
//! a reason one of those two moved:
//!
//! - **`token_id`** — the mask is the principal's. Never the bearer token itself, and never reused
//!   within a process (`crate::cache::RowProjectionKey`'s fact 1 is what makes that sound).
//! - **`view`**, **`layer`**, **`level`** — what the column is *of*.
//! - **`level_version`** — a publication adds artifacts the column has never labelled, so a stale
//!   histogram is short for the new ones and, worse, is indexed by ordinals that have since moved.
//! - **`segments_version`** — row ids mean something only within one geometry.
//! - **`overlay_version`**, and this is the disclosure-adjacent one. A suppression or a deletion
//!   removes rows from the composed mask, so a count taken before it is **high** — an artifact
//!   served with a number counting documents the viewer may no longer see, and possibly one served
//!   at all where its existence criterion would now fail it. `annotation-write-cycle.md` §3.4 puts
//!   the response to a deny at **accept**, so the correction may not wait for a refresh: the
//!   overlay's own counter moves at the acknowledgement's publication, and a key carrying it cannot
//!   name an entry taken before it. An **unsuppress re-derives** rather than subtracts, which is why
//!   `delete → suppress → unsuppress` leaves the entity deleted here too — this cache never
//!   subtracts anything, it only rotates keys.
//! - **the fragment's identity and watermark** — a session may be served a one-generation-stale
//!   projection (decision 0044), so two requests at one `segments_version` can compose against
//!   different fragments. A histogram is per fragment or it is a count over someone else's idea of
//!   the session's own visible set.
//!
//! **The filter is deliberately not a term**, and must not become one: the count beside an artifact
//! is filter-blind (**I12** — a filter may move the frontier up, never down), so a filtered request
//! and an unfiltered one at the same key want the same histogram.
//!
//! # Removal only, and a byte budget
//!
//! Nothing here mutates a cached value. Eviction and `forget` only ever *remove*, so a rebuilt
//! histogram is identical to the evicted one — the miss path walks the same composed mask over the
//! same column, both of which the key names. That is the row-projection cache's own rule
//! (`crate::cache`), and it is what makes a byte budget a residency policy rather than a
//! correctness one.
//!
//! The budget is ~4 B per artifact for an entry of counts alone, which is 4 MB at 10⁶ artifacts and
//! 40 MB at 10⁷ — the figures 0093 quotes — and it is answered to by eviction of the least recently
//! used entry, exactly as the row-projection cache's is.
//!
//! **An entry carrying the accumulated geometry is 36 B an artifact, nine times that**: a `u32`
//! count, two `u64` sums and four `u32` box bounds beside the 4 B count
//! ([`MaskedGeometry`]). At the rung 6 corpus's 1.65×10⁶ artifacts that is **59 MB a level per
//! session** against the counts' 6.6, and against `tessera-server`'s 256 MB default bound. **The
//! default does not move for it**: the bound is a residency policy and not a correctness one — an
//! entry too large for the whole bound is handed to its caller and simply not admitted — so a
//! deployment that holds fewer levels resident is slower and never wrong, and raising the default
//! would spend memory on every deployment for the few that hold several such levels at once. An
//! operator who wants them resident raises `selection.masked_count_cache_bytes`.
//!
//! **Two entries for one level is by design, not a duplication.** A viewport over a layer deriving
//! a centroid or a box wants the geometry; a browse page over the same level wants the counts and
//! nothing else, and building the geometry for it would read a position per visible row for a
//! number no browse row carries. The `geometry` term of the key is what keeps the two apart, so a
//! browse page cannot be handed an entry without the geometry a viewport then needs, nor made to
//! pay for one it does not.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// What one histogram is a function of. See the module doc: every term is a reason the composed
/// mask or the column moved.
///
/// **Named fields rather than a tuple**, on `crate::cache::RowProjectionKey`'s argument: four of the
/// terms are `u64`, so a transposition at the construction site would compile, run, and key one
/// principal's counts under another's — cross-principal count reuse presenting as a cache-hit-rate
/// improvement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MaskedCountKey {
    /// The session's process-local identity, never the bearer token itself.
    pub token_id: u64,
    pub view: String,
    pub layer: String,
    pub level: u32,
    /// The level's artifact-write counter — what the column and the ordinals are valid for.
    pub level_version: u64,
    /// Whether the entry carries the accumulated geometry beside the counts
    /// ([`MaskedGeometry`]). A term of the key rather than something a caller checks for, so an
    /// entry built for a caller that wanted counts alone can never be handed to one that wants
    /// the geometry and find it absent.
    pub geometry: bool,
    /// The geometry this row space belongs to.
    pub segments_version: u64,
    /// The overlay's own counter, bumped by every deny publication. See the module doc.
    pub overlay_version: u64,
    /// The bundle identity of the fragment cache that produced the session's fragment.
    pub fragment_identity: [u8; 32],
    /// The fragment's watermark.
    pub fragment_watermark: u64,
}

/// Everything about one request's composed mask that a histogram is a function of, gathered once
/// per request rather than at each level.
///
/// **A value rather than five arguments**, on [`MaskedCountKey`]'s argument one call earlier: four
/// of the five are `u64`, and a transposition among them would key one principal's counts under
/// another's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MaskIdentity {
    pub token_id: u64,
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
            token_id: self.token_id,
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
    /// **The two accumulated derived properties, where the level is served from its column alone**
    /// — see [`MaskedGeometry`]. `None` on every other level, whose derived content is computed
    /// from the artifact's own row bitmap one artifact at a time.
    geometry: Option<MaskedGeometry>,
}

/// Per ordinal, `membership ∩ M_auth`'s position sum and bounding box — the input a centroid and a
/// box are functions of, accumulated in the same pass as the counts
/// ([`crate::row_column::RowColumn::accumulate_over`]).
///
/// **Per `(session, level)` and not per artifact**, which is the whole reason it exists: a level
/// served from its column alone has no per-artifact membership, and taking one artifact's rows out
/// of the column costs the visible rows inside its extent — the whole visible set for a scattered
/// artifact. One pass answers for every artifact of the level at once.
///
/// **A derived property is still a function of `membership ∩ M_auth` and of nothing else**
/// (`annotations.md` §4.2): the pass reads only rows the composed mask admits, so an artifact's sum
/// and box are over exactly the members this viewer may see.
#[derive(Debug)]
pub struct MaskedGeometry {
    /// How many of the counted rows the row space could place — the divisor for the mean. Not the
    /// masked count, which counts every visible row.
    placed: Vec<u32>,
    /// Exact: see [`crate::row_column::LevelAccumulation::sums`].
    sums: Vec<[u64; 2]>,
    boxes: Vec<[u32; 4]>,
}

impl MaskedGeometry {
    pub(crate) fn new(placed: Vec<u32>, sums: Vec<[u64; 2]>, boxes: Vec<[u32; 4]>) -> Self {
        MaskedGeometry {
            placed,
            sums,
            boxes,
        }
    }

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
    pub(crate) fn new(counts: Vec<u32>) -> Self {
        MaskedCounts {
            counts,
            geometry: None,
        }
    }

    /// The same counts with the accumulated geometry beside them — see [`MaskedGeometry`].
    pub(crate) fn with_geometry(counts: Vec<u32>, geometry: MaskedGeometry) -> Self {
        MaskedCounts {
            counts,
            geometry: Some(geometry),
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

/// The gauges an operator reads. Names no artifact and no principal — a count of structures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaskedCountStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Bytes currently held.
    pub resident_bytes: u64,
    /// Entries currently held.
    pub entries: usize,
}

struct Entry {
    counts: Arc<MaskedCounts>,
    /// Monotone stamp, for the least-recently-used choice.
    touched: u64,
}

#[derive(Default)]
struct Inner {
    entries: BTreeMap<MaskedCountKey, Entry>,
    resident: u64,
    clock: u64,
}

/// The cache itself: `(session, layer, level)` → masked counts, under a byte bound.
pub struct MaskedCountCache {
    inner: Mutex<Inner>,
    bound_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
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
    /// `bound_bytes` is the resident-byte ceiling. `u64::MAX` means "no bound" — what every
    /// construction site outside the server gets until [`Self::set_bound_bytes`] is called.
    pub fn new(bound_bytes: u64) -> Self {
        MaskedCountCache {
            inner: Mutex::new(Inner::default()),
            bound_bytes: AtomicU64::new(bound_bytes),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Move the ceiling, evicting down to it at once rather than at the next insertion — an
    /// operator lowering a bound wants the memory back.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.bound_bytes.store(bound_bytes, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.evict_to_bound(&mut inner);
    }

    pub fn stats(&self) -> MaskedCountStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        MaskedCountStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            resident_bytes: inner.resident,
            entries: inner.entries.len(),
        }
    }

    /// This key's counts, building them if nothing valid is held.
    ///
    /// **The build runs outside the lock**, so a session's whole-mask walk does not block every
    /// other session's lookup behind it. Two callers racing one key both build and the last one
    /// wins; the key fixes the mask and the column, so the two results are equal and the waste is
    /// one walk rather than a wrong answer.
    ///
    /// An entry larger than the whole bound is **not** admitted, and is returned to the caller
    /// anyway: the request that needs it has it, and the cache does not evict everything else to
    /// hold one level nothing else can fit beside.
    pub(crate) fn get_or_build(
        &self,
        key: MaskedCountKey,
        build: impl FnOnce() -> MaskedCounts,
    ) -> Arc<MaskedCounts> {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.clock += 1;
            let clock = inner.clock;
            if let Some(entry) = inner.entries.get_mut(&key) {
                entry.touched = clock;
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Arc::clone(&entry.counts);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let counts = Arc::new(build());
        let weight = counts.weight_bytes();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if weight <= self.bound_bytes.load(Ordering::Relaxed) {
            inner.clock += 1;
            let touched = inner.clock;
            if let Some(previous) = inner.entries.insert(
                key,
                Entry {
                    counts: Arc::clone(&counts),
                    touched,
                },
            ) {
                inner.resident -= previous.counts.weight_bytes();
            }
            inner.resident += weight;
            self.evict_to_bound(&mut inner);
        }
        counts
    }

    /// Drop everything held for one layer, in every view and for every session.
    ///
    /// **Called when the layer is dropped**, beside `ArtifactProjections::forget` and for the same
    /// reason: a dropped name is tombstoned for ever and the serving path resolves the layer through
    /// the registry before it reaches here, so an entry left behind could never be handed to
    /// anybody — what it could do is stay, at ~40 MB per session per level at the campaign's target.
    ///
    /// The hazard the call site must name is **drop and re-register**: a layer name reused after a
    /// drop must not alias the old layer's cached counts. It cannot, because the level version
    /// restarts and the key carries it — but the entries would still be dead weight, which is what
    /// this removes.
    pub fn forget(&self, layer: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let doomed: Vec<MaskedCountKey> = inner
            .entries
            .keys()
            .filter(|key| key.layer == layer)
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = inner.entries.remove(&key) {
                inner.resident -= entry.counts.weight_bytes();
            }
        }
    }

    /// Remove every histogram belonging to one session. Called when a session is revoked, beside
    /// `RowProjectionCache::prune_token` and with that method's argument: it only removes, so its
    /// worst failure is a needless rebuild, and what makes the keys safe to drop is that a
    /// `token_id` is never reused within a process.
    pub fn prune_token(&self, token_id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let doomed: Vec<MaskedCountKey> = inner
            .entries
            .keys()
            .filter(|key| key.token_id == token_id)
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = inner.entries.remove(&key) {
                inner.resident -= entry.counts.weight_bytes();
            }
        }
    }

    /// Least recently used first, until the bound is met.
    fn evict_to_bound(&self, inner: &mut Inner) {
        let bound = self.bound_bytes.load(Ordering::Relaxed);
        while inner.resident > bound {
            let Some(victim) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = inner.entries.remove(&victim) {
                inner.resident -= entry.counts.weight_bytes();
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(token: u64, layer: &str, overlay: u64) -> MaskedCountKey {
        MaskedCountKey {
            geometry: false,
            token_id: token,
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
        let mut built = 0;
        for _ in 0..3 {
            let held = cache.get_or_build(key(1, "a", 0), || {
                built += 1;
                counts(&[5, 6])
            });
            assert_eq!(held.get(0), 5);
            assert_eq!(held.get(1), 6);
            assert_eq!(held.get(9), 0, "past the level is zero, not a panic");
        }
        assert_eq!(built, 1);
        assert_eq!(cache.stats().hits, 2);
        assert_eq!(cache.stats().misses, 1);
    }

    /// **The deny edge.** A suppression moves the overlay's counter, so the key a request produces
    /// after the acknowledgement is not the key the pre-deny histogram sits under — and the count
    /// the viewer is served is the corrected one on the *next* request rather than at the next
    /// refresh. An unsuppress moves it again and re-derives, which is what leaves
    /// `delete → suppress → unsuppress` deleted.
    #[test]
    fn a_deny_rotates_the_key_rather_than_editing_the_entry() {
        let cache = MaskedCountCache::default();
        let before = cache.get_or_build(key(1, "a", 4), || counts(&[10]));
        assert_eq!(before.get(0), 10);

        // The deny is acknowledged; the overlay publishes; the counter moves.
        let after = cache.get_or_build(key(1, "a", 5), || counts(&[9]));
        assert_eq!(after.get(0), 9, "the corrected count, not the held one");
        // And the pre-deny entry is still exactly what it was — nothing here mutates a value.
        assert_eq!(
            cache.get_or_build(key(1, "a", 4), || counts(&[0])).get(0),
            10
        );
    }

    /// Two principals do not share counts, whatever else their keys have in common.
    #[test]
    fn the_counts_are_the_principals_own() {
        let cache = MaskedCountCache::default();
        assert_eq!(
            cache.get_or_build(key(1, "a", 0), || counts(&[3])).get(0),
            3
        );
        assert_eq!(
            cache.get_or_build(key(2, "a", 0), || counts(&[8])).get(0),
            8
        );
        assert_eq!(
            cache.get_or_build(key(1, "a", 0), || counts(&[0])).get(0),
            3
        );
    }

    /// The budget evicts least-recently-used, and lowering it reclaims at once.
    #[test]
    fn the_budget_bounds_what_is_resident() {
        // Three entries of two counts each: 8 bytes apiece.
        let cache = MaskedCountCache::new(16);
        cache.get_or_build(key(1, "a", 0), || counts(&[1, 1]));
        cache.get_or_build(key(2, "a", 0), || counts(&[2, 2]));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().resident_bytes, 16);

        // Touch the first so the second is the least recently used.
        cache.get_or_build(key(1, "a", 0), || counts(&[0, 0]));
        cache.get_or_build(key(3, "a", 0), || counts(&[3, 3]));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(
            cache
                .get_or_build(key(1, "a", 0), || counts(&[0, 0]))
                .get(0),
            1
        );

        // Lowering the bound reclaims immediately rather than at the next insertion.
        cache.set_bound_bytes(8);
        assert_eq!(cache.stats().entries, 1);
        cache.set_bound_bytes(0);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().resident_bytes, 0);

        // An entry larger than the whole bound is served and not admitted.
        let held = cache.get_or_build(key(4, "a", 0), || counts(&[9, 9]));
        assert_eq!(held.get(0), 9);
        assert_eq!(cache.stats().entries, 0);
    }

    /// A dropped layer's entries go, and one session's revocation takes only that session's.
    #[test]
    fn forget_and_prune_remove_and_only_remove() {
        let cache = MaskedCountCache::default();
        cache.get_or_build(key(1, "a", 0), || counts(&[1]));
        cache.get_or_build(key(1, "b", 0), || counts(&[1]));
        cache.get_or_build(key(2, "a", 0), || counts(&[1]));
        assert_eq!(cache.stats().entries, 3);

        cache.forget("a");
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().resident_bytes, 4);

        cache.get_or_build(key(2, "b", 0), || counts(&[1]));
        cache.prune_token(1);
        assert_eq!(cache.stats().entries, 1);
    }
}
