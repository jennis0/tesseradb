//! The derived-geometry cache: one artifact's centroid, box and hull, held per principal.
//!
//! Deriving a hull is the most expensive thing a response does per artifact — a *measured* p90 of
//! 14 ms and 84 ms on the corpus root, after the input reduction in [`crate::derived`] — and
//! nothing held it. The same request three times running cost 2.4 s, 2.9 s and 2.8 s, and a pan
//! re-serves mostly the same artifacts to the same principal, so the work was being done again for
//! an answer that had not moved.
//!
//! # The key is the whole of the safety, and its first term is the principal
//!
//! A derived property is a function of `membership ∩ M_auth` and of nothing else
//! (`annotations.md` §4.2, **I2** at the artifact), so **every term of the key is a reason that
//! visible set or the artifact's membership moved**. It is `crate::histogram::MaskedCountKey` plus
//! the ordinal and the property set, and for the same reasons term by term:
//!
//! - **`token_id`** — the mask is the principal's, and **a shape is never shared across
//!   principals**. Two viewers of one artifact see two different clouds, so a shape served to the
//!   wrong one describes members that viewer was not served. This is the term that keeps the
//!   caching out of the leak register (`artifact-shapes.md` §10): a hit answers the request that
//!   would have computed the same value. Never the bearer token itself, and never reused within a
//!   process — `crate::cache::RowProjectionKey`'s fact 1 is what makes that sound.
//! - **`view`**, **`layer`**, **`level`**, **`ordinal`** — which artifact, in whose row space.
//! - **`level_version`** — a publication moves the ordinals a level's memberships sit under.
//! - **`segments_version`** — row ids mean something only within one geometry.
//! - **`overlay_version`** — a suppression or a deletion removes rows from the composed mask, so a
//!   shape derived before it is drawn over members the viewer may no longer see. The response to a
//!   deny is at **accept** (`annotation-write-cycle.md` §3.4), so the correction may not wait for a
//!   refresh: the overlay's counter moves at the acknowledgement's publication and a key carrying
//!   it cannot name an entry taken before it.
//! - **the fragment's identity and watermark** — a session may be served a one-generation-stale
//!   projection (decision 0044), so two requests at one `segments_version` can compose against
//!   different fragments.
//! - **the property set** — `/v1/viewport`'s `computed` narrows what is derived
//!   (`artifact-shapes.md` §8 C), so an entry built for `centroid, box` does not answer a request
//!   that also wants the hull. It is the *computed* set and not the request's list: the
//!   intersection with the layer's declaration is taken before this is reached.
//!
//! **The attribute filter is deliberately not a term**, and must not become one. Derived content is
//! taken over `MaskedSet::visible_rows`, which is filter-blind exactly as the masked count beside
//! it is (**I12** — a filter may move the frontier up, never down), so a filtered request and an
//! unfiltered one at the same key want the same geometry. Adding the filter would not be unsafe; it
//! would silently halve the hit rate a viewer typing in a search box gets, which is the case the
//! cache exists for.
//!
//! # Removal only, and a byte budget
//!
//! Nothing here mutates a cached value, exactly as in `crate::cache` and `crate::histogram`:
//! eviction and pruning only ever *remove*, so a rebuilt entry is identical to the evicted one —
//! the miss path derives from the same mask over the same membership, both of which the key names.
//! That is what makes the bound a residency policy rather than a correctness one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::derived::{ComputedProperty, DerivedContent};

/// What one artifact's derived content is a function of. See the module doc: every term is a reason
/// the composed mask, the membership or the request's selection moved.
///
/// **Named fields rather than a tuple**, on `crate::cache::RowProjectionKey`'s argument: five of the
/// terms are `u64`, so a transposition at the construction site would compile, run, and key one
/// principal's geometry under another's — cross-principal shape reuse presenting as a hit-rate
/// improvement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DerivedKey {
    /// The session's process-local identity, never the bearer token itself.
    pub token_id: u64,
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    /// The level's artifact-write counter — what the ordinal is valid for.
    pub level_version: u64,
    /// The geometry this row space belongs to.
    pub segments_version: u64,
    /// The overlay's own counter, bumped by every deny publication. See the module doc.
    pub overlay_version: u64,
    /// The bundle identity of the fragment cache that produced the session's fragment.
    pub fragment_identity: [u8; 32],
    /// The fragment's watermark.
    pub fragment_watermark: u64,
    /// Which properties were computed, as one bit per [`ComputedProperty`] — see
    /// [`properties_bits`].
    pub properties: u8,
}

/// The property set as a bitmask, so the key carries it in a byte rather than a vector.
///
/// **Order-insensitive on purpose.** The declaration's order decides nothing about the values, so
/// two layers declaring the same properties in different orders must not miss each other's entries.
pub(crate) fn properties_bits(declared: &[ComputedProperty]) -> u8 {
    declared.iter().fold(0u8, |bits, property| {
        bits | match property {
            ComputedProperty::Centroid => 1,
            ComputedProperty::Box => 2,
            ComputedProperty::Hull => 4,
        }
    })
}

/// What one entry weighs, in bytes.
///
/// **A floor plus the hull's own vertices.** The floor is what the key, the map slot and the two
/// small fields cost, and it is what stops a bound from bounding no number of entries: a
/// centroid-and-box entry is a few dozen real bytes, so charged honestly a 64 MiB cache would hold
/// millions of them and the map itself would be the memory. Above the floor the hull is the only
/// thing that varies, at 8 bytes a vertex and 4 a ring — the same arithmetic the wire's own figures
/// use.
fn weight_bytes(content: &DerivedContent) -> u64 {
    const FLOOR: u64 = 256;
    let shape = content.shape.as_ref().map_or(0, |parts| {
        parts
            .iter()
            .flat_map(|rings| rings.iter())
            .map(|r| r.len() as u64 * 8 + 4)
            .sum::<u64>()
    });
    FLOOR + shape
}

/// The default resident-byte ceiling, and the one every deployment gets.
///
/// **64 MiB, and there is no configuration key for it.** At the *measured* shapes of the
/// `clusters/hdbscan` layer — a median of 748 bytes and a worst case of about 6 KB on the wire —
/// that is on the order of 60,000 shapes resident, which is every artifact of a large layer for
/// several hundred concurrent principals. The two caches that do carry keys
/// (`serve.row_projection_cache_bytes`, `serve.masked_count_cache_bytes`) do so because their entry
/// size scales with the *corpus*: a row projection is a *measured* 125 MB at 10⁹ items and a
/// masked-count histogram ~4 B per artifact. An entry here is one artifact's outline, whose size is
/// bounded by the vertex budget whatever the corpus does, so the figure an operator would type is
/// one nobody has to compute.
///
/// A miss costs the derivation the cache exists to avoid: a *measured* p50 of 1.6 ms, p90 of
/// 14.4 ms and 84 ms on the 2.42M-member corpus root, gather included.
const DEFAULT_BOUND_BYTES: u64 = 64 * 1024 * 1024;

/// An eviction pass frees this fraction of the bound — `1/8`, 8 MiB at the default — so that a
/// cache running full pays one pass per 32,000 or so inserts rather than one per insert. See
/// [`DerivedCache::evict_to_bound`].
const LOW_WATER_DIVISOR: u64 = 8;

/// The operator gauges. A count of structures, naming no artifact and no principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DerivedCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub resident_bytes: u64,
    pub entries: usize,
}

impl DerivedCacheStats {
    /// Hits as a fraction of lookups, or `None` where nothing has been looked up.
    ///
    /// **The figure that decides whether this cache is worth its bytes**, and the one a pan
    /// sequence is measured on (`tests/hull_geometry.rs`, `the_pan_sequence`): a pan re-serves
    /// mostly the same artifacts to the same principal, so the hit rate in the abstract says
    /// nothing and the hit rate over a pan says everything.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }
}

struct Entry {
    content: Arc<DerivedContent>,
    touched: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<DerivedKey, Entry>,
    resident: u64,
    clock: u64,
}

/// Derived geometry, per `(principal, artifact, mask, property set)`, under a byte bound.
pub struct DerivedCache {
    inner: Mutex<Inner>,
    bound_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl Default for DerivedCache {
    fn default() -> Self {
        Self::new(DEFAULT_BOUND_BYTES)
    }
}

impl std::fmt::Debug for DerivedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DerivedCache")
            .field("stats", &self.stats())
            .finish()
    }
}

impl DerivedCache {
    pub fn new(bound_bytes: u64) -> Self {
        DerivedCache {
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

    pub fn stats(&self) -> DerivedCacheStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        DerivedCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            resident_bytes: inner.resident,
            entries: inner.entries.len(),
        }
    }

    /// This artifact's derived content for this principal, deriving it if nothing is held.
    ///
    /// **The derivation runs outside the lock**, so one session's 2.42M-member root does not block
    /// every other session's lookup behind it. Two callers racing one key both derive and the last
    /// one wins; the key fixes the mask and the membership, so the two results are equal and the
    /// waste is one derivation rather than a wrong answer. There is no single-flight state machine
    /// here for that reason and one more: a miss is tens of milliseconds, not the seconds a row
    /// projection takes, so parking a request on another's build would cost more than repeating it.
    ///
    /// An entry larger than the whole bound is **not** admitted, and is returned to the caller
    /// anyway: the request that needs it has it, and the cache does not evict everything else to
    /// hold one shape nothing else can fit beside.
    pub(crate) fn get_or_derive(
        &self,
        key: DerivedKey,
        derive: impl FnOnce() -> DerivedContent,
    ) -> Arc<DerivedContent> {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.clock += 1;
            let clock = inner.clock;
            if let Some(entry) = inner.entries.get_mut(&key) {
                entry.touched = clock;
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Arc::clone(&entry.content);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let content = Arc::new(derive());
        let weight = weight_bytes(&content);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if weight <= self.bound_bytes.load(Ordering::Relaxed) {
            inner.clock += 1;
            let touched = inner.clock;
            if let Some(previous) = inner.entries.insert(
                key,
                Entry {
                    content: Arc::clone(&content),
                    touched,
                },
            ) {
                inner.resident -= weight_bytes(&previous.content);
            }
            inner.resident += weight;
            self.evict_to_bound(&mut inner);
        }
        content
    }

    /// Remove every entry belonging to one session. Called when a session is revoked, beside
    /// `RowProjectionCache::prune_token` and with that method's argument: it only removes, so its
    /// worst failure is a needless rebuild, and what makes the keys safe to drop is that a
    /// `token_id` is never reused within a process.
    pub fn prune_token(&self, token_id: u64) {
        self.prune_tokens(&std::iter::once(token_id).collect());
    }

    /// The same removal for a set of sessions, in one pass — the expiry sweep's form. See
    /// `RowProjectionCache::prune_tokens` for why a batch is not a loop over single removals.
    pub fn prune_tokens(&self, token_ids: &rustc_hash::FxHashSet<u64>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let doomed: Vec<DerivedKey> = inner
            .entries
            .keys()
            .filter(|key| token_ids.contains(&key.token_id))
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = inner.entries.remove(&key) {
                inner.resident -= weight_bytes(&entry.content);
            }
        }
    }

    /// Least recently used first, once the bound is crossed, down to [`LOW_WATER`] of it.
    ///
    /// **Victims are chosen in one pass rather than one at a time**, which is the difference
    /// between this and `crate::histogram`'s otherwise identical eviction. That cache holds one
    /// entry per `(session, layer, level)` and a scan per victim is nothing; this one holds an
    /// entry per *artifact*, so a scan per victim is quadratic in a residency that a whole-layer
    /// response fills a few hundred entries at a time.
    ///
    /// **And the pass evicts a batch, not one entry.** The pass is a clone and a sort of every key
    /// held — a quarter of a million at the default bound — and an insert that lands on a full
    /// cache used to run it to free exactly its own weight, so the *next* insert ran it again.
    /// GeoNames' `admin/hierarchy` has 464,000 artifacts, more than the bound holds at the entry
    /// floor; once one principal had been served its deeper levels, every further insert paid a
    /// full pass, three abandoned requests spent minutes of CPU inside this lock, and a request
    /// that took 21 ms on a fresh process took 4–6 s behind them and 60 s at the next level down
    /// (2026-08-28). Evicting to a low-water mark makes the pass amortised: one per batch of
    /// inserts, not one per insert.
    fn evict_to_bound(&self, inner: &mut Inner) {
        let bound = self.bound_bytes.load(Ordering::Relaxed);
        if inner.resident <= bound {
            return;
        }
        let target = bound - bound / LOW_WATER_DIVISOR;
        let mut by_age: Vec<(u64, DerivedKey)> = inner
            .entries
            .iter()
            .map(|(key, entry)| (entry.touched, key.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(touched, _)| *touched);
        for (_, key) in by_age {
            if inner.resident <= target {
                break;
            }
            if let Some(entry) = inner.entries.remove(&key) {
                inner.resident -= weight_bytes(&entry.content);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(token: u64, ordinal: u32, overlay: u64) -> DerivedKey {
        DerivedKey {
            token_id: token,
            view: "s0".into(),
            layer: "clusters/hdbscan".into(),
            level: 0,
            ordinal,
            level_version: 1,
            segments_version: 1,
            overlay_version: overlay,
            fragment_identity: [7u8; 32],
            fragment_watermark: 0,
            properties: properties_bits(&[ComputedProperty::Hull]),
        }
    }

    /// A full cache evicts a batch per pass, not one entry per insert: the insert that crosses the
    /// bound lands the residency at the low-water mark, and the inserts that follow land nothing
    /// on the floor until the bound is crossed again.
    #[test]
    fn a_full_cache_evicts_a_batch_per_pass() {
        let floor = weight_bytes(&DerivedContent::default());
        let bound = 256 * floor;
        let cache = DerivedCache::new(bound);
        for i in 0..257u32 {
            cache.get_or_derive(key(1, i, 0), DerivedContent::default);
        }
        let after_first_pass = cache.stats();
        assert!(after_first_pass.evictions > 1, "one pass evicted {}", after_first_pass.evictions);
        assert!(after_first_pass.resident_bytes <= bound - bound / LOW_WATER_DIVISOR);
        // The next inserts ride the room the pass made: no pass, no eviction.
        for i in 257..(257 + 16) {
            cache.get_or_derive(key(1, i, 0), DerivedContent::default);
        }
        assert_eq!(cache.stats().evictions, after_first_pass.evictions);
        assert!(cache.stats().resident_bytes <= bound);
    }

    fn shape(x: u32) -> DerivedContent {
        DerivedContent {
            shape: Some(vec![vec![vec![[x, 0], [x + 1, 0], [x, 1]]]]),
            ..Default::default()
        }
    }

    #[test]
    fn a_hit_does_not_derive_and_a_miss_does() {
        let cache = DerivedCache::default();
        let mut derived = 0;
        for _ in 0..3 {
            let held = cache.get_or_derive(key(1, 4, 0), || {
                derived += 1;
                shape(10)
            });
            assert_eq!(held.shape, shape(10).shape);
        }
        assert_eq!(derived, 1);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (2, 1));
        assert_eq!(stats.hit_rate(), Some(2.0 / 3.0));
    }

    /// **Two principals never share a shape**, which is the term the whole disclosure argument for
    /// this cache rests on: a hull is derived from `membership ∩ M_auth`, so one principal's is not
    /// an answer to another's request even for the same artifact.
    #[test]
    fn a_second_principal_derives_its_own() {
        let cache = DerivedCache::default();
        let first = cache.get_or_derive(key(1, 4, 0), || shape(10));
        let second = cache.get_or_derive(key(2, 4, 0), || shape(20));
        assert_eq!(first.shape, shape(10).shape);
        assert_eq!(
            second.shape,
            shape(20).shape,
            "one principal's shape answered another's request"
        );
        assert_eq!(cache.stats().misses, 2);
    }

    /// **The deny edge**, exactly as `crate::histogram`'s: a suppression moves the overlay's
    /// counter, so the key a request produces after the acknowledgement is not the key the pre-deny
    /// shape sits under. The viewer gets the corrected outline on the next request rather than at
    /// the next refresh, and nothing here mutates the held one.
    #[test]
    fn a_deny_rotates_the_key_rather_than_editing_the_entry() {
        let cache = DerivedCache::default();
        let before = cache.get_or_derive(key(1, 4, 0), || shape(10));
        assert_eq!(before.shape, shape(10).shape);
        let after = cache.get_or_derive(key(1, 4, 1), || shape(20));
        assert_eq!(
            after.shape,
            shape(20).shape,
            "the pre-deny shape was served after the deny"
        );
    }

    /// A request asking for fewer properties does not take an entry built for more, and does not
    /// overwrite it either — `computed` narrows what is derived, and the two answers differ.
    #[test]
    fn the_property_set_is_part_of_the_key() {
        let cache = DerivedCache::default();
        let mut hull_key = key(1, 4, 0);
        hull_key.properties = properties_bits(&[ComputedProperty::Hull]);
        let mut box_key = key(1, 4, 0);
        box_key.properties = properties_bits(&[ComputedProperty::Box]);
        cache.get_or_derive(hull_key, || shape(10));
        let boxed = cache.get_or_derive(box_key, || DerivedContent {
            bbox: Some([0, 0, 1, 1]),
            ..Default::default()
        });
        assert!(
            boxed.shape.is_none(),
            "a hull was served to a request that did not ask for one"
        );
        assert_eq!(cache.stats().misses, 2);
    }

    /// The declaration's order decides nothing, so two orders of the same properties are one key.
    #[test]
    fn the_property_set_does_not_depend_on_the_declaration_order() {
        assert_eq!(
            properties_bits(&[ComputedProperty::Hull, ComputedProperty::Box]),
            properties_bits(&[ComputedProperty::Box, ComputedProperty::Hull])
        );
    }

    /// The bound evicts, least recently used first, and the gauge says so.
    #[test]
    fn the_bound_evicts_the_least_recently_used() {
        // Three entries' worth, at the 256-byte floor plus one ring of three vertices — with enough
        // over the third that the low-water mark (an eighth below the bound) still holds three, so
        // the fourth's arrival evicts exactly the oldest and not a batch.
        let cache = DerivedCache::new(3 * (256 + 28) + 148);
        for ordinal in 0..3 {
            cache.get_or_derive(key(1, ordinal, 0), || shape(ordinal));
        }
        assert_eq!(cache.stats().entries, 3);
        // Touch 0 and 1 so 2 is the oldest, then admit a fourth.
        cache.get_or_derive(key(1, 0, 0), || shape(0));
        cache.get_or_derive(key(1, 1, 0), || shape(1));
        cache.get_or_derive(key(1, 3, 0), || shape(3));
        assert_eq!(cache.stats().entries, 3);
        assert_eq!(cache.stats().evictions, 1);
        let mut derived = false;
        cache.get_or_derive(key(1, 2, 0), || {
            derived = true;
            shape(2)
        });
        assert!(derived, "the least recently used entry was not the victim");
    }

    /// A revoked session's shapes go with it — memory hygiene, on `prune_token`'s own argument.
    #[test]
    fn a_revoked_session_leaves_nothing_resident() {
        let cache = DerivedCache::default();
        cache.get_or_derive(key(1, 4, 0), || shape(10));
        cache.get_or_derive(key(2, 4, 0), || shape(20));
        cache.prune_token(1);
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().resident_bytes, 256 + 28);
    }
}
