//! The derived-geometry cache: one artifact's centroid, box and hull, held per principal.
//!
//! The mechanism — the byte bound, LRU eviction, the per-entry floor and the single-flight slot —
//! is [`tessera_cache::SingleFlightCache`]. What lives here is the key, the weight and the two
//! pruners.
//!
//! # The key is the whole of the safety, and its first term is the principal
//!
//! A derived property is a function of `membership ∩ M_auth` and of nothing else, so **every term
//! of the key is a reason that visible set or the artifact's membership moved**:
//!
//! - **`token_id`** — the mask is the principal's, and **a shape is never shared across
//!   principals**. Two viewers of one artifact see two different clouds, so a shape served to the
//!   wrong one describes members that viewer was not served. Never the bearer token itself, and
//!   never reused within a process — `crate::cache::RowProjectionKey`'s fact 1 is what makes that
//!   sound.
//! - **`view`**, **`layer`**, **`level`**, **`ordinal`** — which artifact, in whose row space.
//! - **`level_version`** — a publication moves the ordinals a level's memberships sit under.
//! - **`segments_version`** — row ids mean something only within one geometry.
//! - **`overlay_version`** — a suppression or a deletion removes rows from the composed mask, so a
//!   shape derived before it is drawn over members the viewer may no longer see. The counter moves
//!   at the acknowledgement's publication, so a key carrying it cannot name an entry taken before
//!   the deny, and the correction does not wait for a refresh.
//! - **the fragment's identity and watermark** — a session may be served a one-generation-stale
//!   projection, so two requests at one `segments_version` can compose against different fragments.
//! - **the property set** — a request narrows what is derived, so an entry built for `centroid,
//!   box` does not answer a request that also wants the hull. It is the *computed* set and not the
//!   request's list: the intersection with the layer's declaration is taken before this is reached.
//!
//! **The attribute filter is deliberately not a term**, and must not become one. Derived content is
//! taken over `MaskedSet::visible_rows`, which is filter-blind exactly as the masked count beside
//! it is, so a filtered request and an unfiltered one at the same key want the same geometry.
//! Adding the filter would not be unsafe; it would silently halve the hit rate a viewer typing in a
//! search box gets, which is the case the cache exists for.
//!
//! Nothing here mutates a cached value: eviction and pruning only ever *remove*, so a rebuilt entry
//! is identical to the evicted one — the miss path derives from the same mask over the same
//! membership, both of which the key names. That is what makes the bound a residency policy rather
//! than a correctness one.

use std::sync::Arc;

use tessera_cache::{CacheWeight, SingleFlightCache};

use crate::derived::{ComputedProperty, DerivedContent};

/// What one artifact's derived content is a function of. See the module doc: every term is a reason
/// the composed mask, the membership or the request's selection moved.
///
/// **Named fields rather than a tuple**, on `crate::cache::RowProjectionKey`'s argument: five of
/// the terms are `u64`, so a transposition at the construction site would compile, run, and key one
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

impl CacheWeight for DerivedContent {
    /// **The hull's own vertices**, at 8 bytes a vertex and 4 a ring — the same arithmetic the
    /// wire's own figures use. Above the per-entry floor the hull is the only thing that varies;
    /// a centroid-and-box entry is a few dozen real bytes and is charged
    /// `tessera_cache::PER_ENTRY_FLOOR_BYTES`.
    fn cache_weight_bytes(&self) -> u64 {
        self.shape.as_ref().map_or(0, |parts| {
            parts
                .iter()
                .flat_map(|rings| rings.iter())
                .map(|ring| ring.len() as u64 * 8 + 4)
                .sum::<u64>()
        })
    }
}

/// The default resident-byte ceiling, and the one every deployment gets.
///
/// **64 MiB, and there is no configuration key for it.** At the *measured* shapes of the
/// `clusters/hdbscan` layer — a median of 748 bytes and a worst case of about 6 KB on the wire —
/// that is tens of thousands of shapes resident, which is every artifact of a large layer for
/// several hundred concurrent principals. The two caches that do carry keys
/// (`serve.row_projection_cache_bytes`, `serve.masked_count_cache_bytes`) do so because their entry
/// size scales with the *corpus*. An entry here is one artifact's outline, whose size is bounded by
/// the vertex budget whatever the corpus does, so the figure an operator would type is one nobody
/// has to compute.
///
/// A miss costs the derivation the cache exists to avoid: a *measured* p50 of 1.6 ms, p90 of
/// 14.4 ms and 84 ms on the 2.42M-member corpus root, gather included.
const DEFAULT_BOUND_BYTES: u64 = 64 * 1024 * 1024;

/// The operator gauges. A count of structures, naming no artifact and no principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DerivedCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub resident_bytes: u64,
    /// Slots held, a derivation in flight counted as well as a held shape.
    pub entries: usize,
}

impl DerivedCacheStats {
    /// Hits as a fraction of lookups, or `None` where nothing has been looked up.
    ///
    /// **The figure that decides whether this cache is worth its bytes**, and the one a pan
    /// sequence is measured on: a pan re-serves mostly the same artifacts to the same principal, so
    /// the hit rate in the abstract says nothing and the hit rate over a pan says everything.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }
}

/// Derived geometry, per `(principal, artifact, mask, property set)`, under a byte bound.
pub struct DerivedCache {
    inner: SingleFlightCache<DerivedKey, DerivedContent>,
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
            inner: SingleFlightCache::new(bound_bytes),
        }
    }

    /// Move the ceiling. Residents over it go at the next admission.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.inner.set_bound_bytes(bound_bytes);
    }

    pub fn stats(&self) -> DerivedCacheStats {
        let stats = self.inner.stats();
        DerivedCacheStats {
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            resident_bytes: stats.bytes,
            entries: stats.entries,
        }
    }

    /// This artifact's derived content for this principal, deriving it if nothing is held.
    ///
    /// **The derivation runs outside the lock**, so one session's 2.42M-member root does not block
    /// every other session's lookup behind it.
    ///
    /// **A caller that finds another's derivation in flight derives its own and caches nothing**,
    /// rather than parking on it. The key fixes the mask and the membership, so the two results are
    /// equal and the waste is one derivation; a miss is tens of milliseconds, not the seconds a row
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
        let mut derive = Some(derive);
        let held = self.inner.get_or_derive(key, None, |_| {
            (derive.take().expect("the builder runs once"))()
        });
        match held {
            Ok(content) => content,
            Err(_building) => Arc::new((derive.take().expect("the builder runs once"))()),
        }
    }

    /// Remove every entry belonging to one session. Called when a session is revoked: it only
    /// removes, so its worst failure is a needless rebuild, and what makes the keys safe to drop is
    /// that a `token_id` is never reused within a process.
    pub fn prune_token(&self, token_id: u64) {
        self.inner.retain_keys(|key| key.token_id != token_id);
    }

    /// The same removal for a set of sessions, in one pass — the expiry sweep's form. See
    /// `RowProjectionCache::prune_tokens` for why a batch is not a loop over single removals.
    pub fn prune_tokens(&self, token_ids: &rustc_hash::FxHashSet<u64>) {
        self.inner
            .retain_keys(|key| !token_ids.contains(&key.token_id));
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

    /// **The deny edge**: a suppression moves the overlay's counter, so the key a request produces
    /// after the acknowledgement is not the key the pre-deny shape sits under. The viewer gets the
    /// corrected outline on the next request rather than at the next refresh, and nothing here
    /// mutates the held one.
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

    /// A revoked session's shapes go with it — memory hygiene, on `prune_token`'s own argument.
    #[test]
    fn a_revoked_session_leaves_nothing_resident() {
        let cache = DerivedCache::default();
        cache.get_or_derive(key(1, 4, 0), || shape(10));
        cache.get_or_derive(key(2, 4, 0), || shape(20));
        cache.prune_token(1);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.resident_bytes, tessera_cache::PER_ENTRY_FLOOR_BYTES);
    }

    /// A second caller arriving while a key is being derived derives its own rather than waiting,
    /// and the winner's entry is what is held afterwards.
    #[test]
    fn a_concurrent_miss_derives_its_own_and_caches_nothing() {
        use std::sync::mpsc;

        let cache = Arc::new(DerivedCache::default());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let winner_cache = Arc::clone(&cache);
        let winner = std::thread::spawn(move || {
            winner_cache.get_or_derive(key(1, 4, 0), move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                shape(10)
            })
        });
        started_rx.recv().unwrap();

        let loser = cache.get_or_derive(key(1, 4, 0), || shape(20));
        assert_eq!(loser.shape, shape(20).shape, "the loser derives its own");

        release_tx.send(()).unwrap();
        assert_eq!(winner.join().unwrap().shape, shape(10).shape);
        let held = cache.get_or_derive(key(1, 4, 0), || panic!("the winner's entry is held"));
        assert_eq!(held.shape, shape(10).shape);
    }
}
