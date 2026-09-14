//! **The per-session visible-value set** — the suggestion verb's second route
//! (`value-suggestion.md` §6.3, [decision 0124](../../../docs/decisions/0124-the-suggestion-route-may-follow-the-viewers-cardinality.md)).
//!
//! Where a viewer's composed candidate is small enough to sweep, a keystroke stops probing one
//! memory-mapped posting per value walked and reads a bit instead: a Roaring bitmap over the
//! vocabulary's **dense value positions**, computed by one pass over the column's entity-space
//! value column under that viewer's own candidate.
//!
//! # What the key is, and why every term of it is there
//!
//! `(token_id, resolved column, segments_version, overlay_version)` — the design's own four, and
//! the first two are the ordinary ones: the set is a function of the principal's mask, and of the
//! column whose values it is over (a group-scoped family is one column per view, so the *resolved*
//! spelling is the address, exactly as it is on the two listing verbs).
//!
//! The two versions are the safety. **A set whose key does not match the live generation and
//! overlay version is never served** — it is not stale-but-usable, it is fail-open: a suppression
//! accepted since the sweep removes the last visible member of a value, and a set taken before it
//! would keep offering that value's *name*, which is exactly the C11 disclosure `derived` exists to
//! withhold. `overlay_version` moves at a deny's publication and `segments_version` at every flush,
//! so a key carrying both cannot name an entry taken before either. The cost is the one §6.3 names:
//! an entry goes cold on every flush and every deny.
//!
//! **A miss is never an error.** The probe route of §6.2 answers every request the set does not,
//! and the two produce the same page under the same gate — so nothing here can withhold a value or
//! offer one. What it changes is what a keystroke *costs* and what its timing carries.
//!
//! # Removal only, and a byte budget
//!
//! Nothing here mutates an entry: eviction and pruning only ever *remove*, so a rebuilt set is
//! identical to the evicted one — the key fixes the mask, the column and the generation, all three
//! of which the sweep is a function of. That is what makes the bound a residency policy rather than
//! a correctness one, and it is [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
//! byte budget applied to this cache exactly as `crate::derived_cache` applies it to shapes.
//!
//! **Nothing is materialised per session in advance**, which is 0093's actual rule: the first
//! suggest on a `(session, column)` pair dispatches the sweep on the pool and is itself answered by
//! the probe route, as is every keystroke until it lands.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use croaring::{Bitmap, Portable};

/// What one session's visible-value set is a function of. See the module doc: every term is a
/// reason the composed mask or the vocabulary's membership moved.
///
/// **Named fields rather than a tuple**, on `crate::derived_cache::DerivedKey`'s argument: three of
/// the four are `u64`, so a transposition at the construction site would compile, run, and key one
/// principal's visible values under another's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SuggestSetKey {
    /// The session's process-local identity, never the bearer token itself. Never reused within a
    /// process, which is what makes `prune_token` sound.
    pub token_id: u64,
    /// The **resolved** column — one view's, under a group-scoped family.
    pub column: String,
    /// The geometry this generation published; moved by every flush.
    pub segments_version: u64,
    /// The overlay's own counter, bumped by every deny publication. See the module doc.
    pub overlay_version: u64,
}

/// One session's visible values, as dense positions in its vocabulary's index.
#[derive(Debug)]
pub struct SuggestSet {
    positions: Bitmap,
    /// The vocabulary's value count when the sweep ran — carried so a set built against one index
    /// cannot be read against a later, longer one. A rebuild mints new positions for the values it
    /// appends and keeps the ones before them, so this is a **guard against a rebuild that
    /// reordered**, not a correctness dependency of the ordinary case.
    values: u32,
}

impl SuggestSet {
    pub(crate) fn new(positions: Bitmap, values: u32) -> Self {
        SuggestSet { positions, values }
    }

    /// Whether this viewer can see the value at `position`.
    ///
    /// **The one question the walk asks of it**, and it is the whole gate on this route: a value
    /// whose bit is clear is not offered, exactly as a value whose posting misses the candidate is
    /// not.
    pub fn contains(&self, position: u32) -> bool {
        self.positions.contains(position)
    }

    /// How many values this viewer can see in this vocabulary — an operator/bench observable.
    pub fn visible_values(&self) -> u64 {
        self.positions.cardinality()
    }

    /// The value count the sweep ran against.
    pub fn values(&self) -> u32 {
        self.values
    }

    /// The bitmap's serialised size — the reading the byte budget charges an entry, exposed so a
    /// bench prices the residency the design's own arm 3 reports rather than modelling it.
    pub fn serialized_bytes(&self) -> u64 {
        self.positions.get_serialized_size_in_bytes::<Portable>() as u64
    }

    fn weight_bytes(&self) -> u64 {
        // A floor for the key, the map slot and the `Arc`, so a bound bounds a number of entries
        // and not only their payloads — `crate::derived_cache::weight_bytes`' argument.
        const FLOOR: u64 = 256;
        FLOOR + self.serialized_bytes()
    }
}

/// The default resident-byte ceiling.
///
/// **64 MiB, and no configuration key for it**, on `crate::derived_cache`'s argument: a set's size
/// is bounded by the *vocabulary* and not by the corpus — `V` bits, a **measured** 1.25 MB at 10⁷
/// values once a fifth of them are visible and 13 KB at 0.06%
/// (`probes/2026-09-02-value-suggestion/` arm 3) — so the figure an operator would type is one
/// nobody has to compute. A miss costs the probe route, which is a *measured* 61–68 ms for the
/// sparsest viewer at 10⁷ values and under a millisecond for everyone else: slower, never wrong.
const DEFAULT_BOUND_BYTES: u64 = 64 * 1024 * 1024;

/// An eviction pass frees this fraction of the bound, so a full cache pays one pass per batch of
/// inserts rather than one per insert (`crate::derived_cache::LOW_WATER_DIVISOR`'s argument).
const LOW_WATER_DIVISOR: u64 = 8;

/// The operator gauges. Counts of structures, naming no principal and no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SuggestSetStats {
    /// Requests answered from a set — C31 closed for that keystroke.
    pub hits: u64,
    /// Requests that found no set and took the probe route.
    pub misses: u64,
    /// Sweeps that completed and were admitted.
    pub builds: u64,
    /// Requests that found no set and started no sweep, because the viewer's own cardinality is
    /// over `selection.max_suggest_set_entities` or the column has nothing to sweep.
    pub declined: u64,
    /// Entries dropped by the value-count guard — a set swept against an index a rebuild has since
    /// superseded. Distinct from an eviction: nothing about residency caused it, and the pair it
    /// belonged to sweeps again on the same request.
    pub discarded: u64,
    pub evictions: u64,
    pub resident_bytes: u64,
    pub entries: usize,
    /// Sweeps in flight.
    pub in_flight: usize,
}

struct Entry {
    set: std::sync::Arc<SuggestSet>,
    touched: u64,
    bytes: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<SuggestSetKey, Entry>,
    /// Keys a sweep is running for — §6.3's rule 2, one build in flight per key.
    in_flight: HashSet<SuggestSetKey>,
    resident: u64,
    clock: u64,
}

/// Per-session visible-value sets, under a byte bound.
pub struct SuggestSets {
    inner: Mutex<Inner>,
    bound_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    builds: AtomicU64,
    declined: AtomicU64,
    discarded: AtomicU64,
    evictions: AtomicU64,
}

impl Default for SuggestSets {
    fn default() -> Self {
        Self::new(DEFAULT_BOUND_BYTES)
    }
}

impl std::fmt::Debug for SuggestSets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuggestSets")
            .field("stats", &self.stats())
            .finish()
    }
}

impl SuggestSets {
    pub fn new(bound_bytes: u64) -> Self {
        SuggestSets {
            inner: Mutex::new(Inner::default()),
            bound_bytes: AtomicU64::new(bound_bytes),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            builds: AtomicU64::new(0),
            declined: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Move the ceiling, evicting down to it at once rather than at the next insertion.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.bound_bytes.store(bound_bytes, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.evict_to_bound(&mut inner);
    }

    pub fn stats(&self) -> SuggestSetStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        SuggestSetStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            declined: self.declined.load(Ordering::Relaxed),
            discarded: self.discarded.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            resident_bytes: inner.resident,
            entries: inner.entries.len(),
            in_flight: inner.in_flight.len(),
        }
    }

    /// The set for this exact key, swept against an index of exactly `values` values, or `None`.
    ///
    /// **Exact, and that is the disclosure control** (module doc): a key differing only in
    /// `overlay_version` names a set taken before a suppression, and answering with it would offer
    /// a value whose last visible member the viewer may no longer see. There is deliberately no
    /// nearest-match, no fallback and no "close enough" — a miss costs the probe route.
    ///
    /// **`values` is the second half of that, and the key cannot carry it.** A suggestion-index
    /// rebuild appends what the side map minted and re-sorts, which moves the dense positions of
    /// everything after each insertion — so a set swept against the older, shorter index would name
    /// *other values'* positions. A rebuild publishes its own generation and deliberately moves
    /// neither `segments_version` nor `overlay_version` (`crate::write`'s
    /// `publish_completed_suggests`), so nothing in the key sees it; the vocabulary only ever grows,
    /// so the value count does.
    ///
    /// **A rejected entry is dropped here rather than left to expire.** Left in place it would be
    /// found by [`Self::claim`], which refuses to sweep for a key it holds a set for — so the pair
    /// would stay on the probe route until the key rotated or eviction reached it, which for a
    /// steady session is neither. Dropping it at the point of rejection makes the same request that
    /// found it stale the one that starts the replacement.
    pub(crate) fn get(
        &self,
        key: &SuggestSetKey,
        values: u32,
    ) -> Option<std::sync::Arc<SuggestSet>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.clock += 1;
        let clock = inner.clock;
        match inner.entries.get_mut(key) {
            Some(entry) if entry.set.values() == values => {
                entry.touched = clock;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(std::sync::Arc::clone(&entry.set))
            }
            Some(_) => {
                if let Some(entry) = inner.entries.remove(key) {
                    inner.resident -= entry.bytes;
                }
                self.discarded.fetch_add(1, Ordering::Relaxed);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Claim the sweep for `key`: `true` iff this caller should run it.
    ///
    /// **§6.3's rule 2, one build in flight per key.** A second keystroke arriving while the first
    /// one's sweep runs is answered by the probe route and starts nothing; a claim is released by
    /// [`Self::finish`] or [`Self::abandon`], whichever the sweep reaches.
    pub(crate) fn claim(&self, key: &SuggestSetKey) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.entries.contains_key(key) {
            return false;
        }
        inner.in_flight.insert(key.clone())
    }

    /// A sweep that never produced a set — the column had nothing to sweep, or a read failed.
    /// Releases the claim so a later request may try again.
    pub(crate) fn abandon(&self, key: &SuggestSetKey) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.in_flight.remove(key);
    }

    /// Admit a finished sweep, releasing its claim.
    ///
    /// **Every other set for the same `(token_id, column)` is dropped here**, which is §6.3's rule
    /// 1 made to actually free memory rather than merely to withhold: those entries differ from
    /// this one in `segments_version` or `overlay_version`, so nothing will ever ask for them
    /// again, and leaving them resident would hold one bitmap per flush per session until the
    /// eviction pass reached them.
    pub(crate) fn finish(&self, key: SuggestSetKey, set: std::sync::Arc<SuggestSet>) {
        let bytes = set.weight_bytes();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.in_flight.remove(&key);

        let superseded: Vec<SuggestSetKey> = inner
            .entries
            .keys()
            .filter(|held| held.token_id == key.token_id && held.column == key.column)
            .cloned()
            .collect();
        for held in superseded {
            if let Some(entry) = inner.entries.remove(&held) {
                inner.resident -= entry.bytes;
            }
        }

        if bytes > self.bound_bytes.load(Ordering::Relaxed) {
            // Not admitted, and the caller keeps its own copy: the cache does not evict everything
            // else to hold one set nothing can fit beside.
            return;
        }
        inner.clock += 1;
        let touched = inner.clock;
        inner.entries.insert(key, Entry { set, touched, bytes });
        inner.resident += bytes;
        self.builds.fetch_add(1, Ordering::Relaxed);
        self.evict_to_bound(&mut inner);
    }

    /// A request that took the probe route and started no sweep — the viewer is wider than the
    /// ceiling, or the column has nothing to sweep.
    pub(crate) fn note_declined(&self) {
        self.declined.fetch_add(1, Ordering::Relaxed);
    }

    /// Remove every entry belonging to one session, on `RowProjectionCache::prune_token`'s
    /// argument: it only removes, so its worst failure is a needless sweep, and a `token_id` is
    /// never reused within a process.
    pub fn prune_token(&self, token_id: u64) {
        self.prune_tokens(&std::iter::once(token_id).collect());
    }

    /// The same removal for a set of sessions, in one pass — the expiry sweep's form. See
    /// `RowProjectionCache::prune_tokens` for why a batch is not a loop over single removals.
    pub fn prune_tokens(&self, token_ids: &rustc_hash::FxHashSet<u64>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let doomed: Vec<SuggestSetKey> = inner
            .entries
            .keys()
            .filter(|key| token_ids.contains(&key.token_id))
            .cloned()
            .collect();
        for key in doomed {
            if let Some(entry) = inner.entries.remove(&key) {
                inner.resident -= entry.bytes;
            }
        }
    }

    /// Least recently used first, once the bound is crossed, down to the low-water mark.
    fn evict_to_bound(&self, inner: &mut Inner) {
        let bound = self.bound_bytes.load(Ordering::Relaxed);
        if inner.resident <= bound {
            return;
        }
        let target = bound - bound / LOW_WATER_DIVISOR;
        let mut by_age: Vec<(u64, SuggestSetKey)> = inner
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
                inner.resident -= entry.bytes;
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// **The sweep** — one pass over a column's entity-space value layers under `candidate`, turned
/// into the dense positions of the values it found (§6.3).
///
/// `None` where the column cannot take the set at all: **rule 4**, a column with no entity-space
/// value column to sweep — a blob-resident `derived` category, which has postings and nothing else
/// — and, fail-closed, a column whose layers are not a category's at all. The all-postings
/// alternative for the first is a *measured* 0.3–6.8 s at every sparsity, because it opens every
/// record whatever the candidate; such a column stays on the probe route or declares `index = true`.
///
/// **Every layer, not only the extents.** `FilterColumns::category_membership` sweeps the extents
/// alone because the postings answer the base, and this route has no postings: the base value
/// column is where nearly every value's membership is. The layers are disjoint in entity space by
/// **I9**, so the order they are visited decides nothing.
///
/// **The codes are sorted and deduplicated before the position lookup**, which is what makes the
/// lookup a warm-started gallop rather than a binary search per entity: at the 10⁷-entity ceiling
/// the sweep visits ten million codes and the vocabulary holds ten million values, so a search per
/// entity would be seconds where a sort plus a merge is a fraction of one.
///
/// **A code with no dense position is skipped**, which is the fail-closed direction: a value minted
/// since the index was built has none, is therefore not in the set, and is answered by the probe
/// route at the walk's side-map arm.
pub fn sweep<'a>(
    columns: impl Iterator<Item = &'a tessera_filter::ValueColumn>,
    candidate: &Bitmap,
    index: &crate::suggest::SuggestIndex,
) -> Option<SuggestSet> {
    let mut codes: Vec<u32> = Vec::new();
    let mut swept = false;
    for column in columns {
        swept = true;
        if !column.visit_codes(candidate, |code| codes.push(code)) {
            return None;
        }
    }
    if !swept {
        return None;
    }
    codes.sort_unstable();
    codes.dedup();

    // A plain `V`-bit word array, then one ascending `add_many` — the *measured* fastest of the
    // three forms arm 3 priced (`add` per entity is 6× slower). `V` bits is 1.25 MB at 10⁷ values
    // and is transient; the Roaring form it becomes is 13 KB for a sparse viewer.
    let values = index.values();
    let mut words = vec![0u64; values as usize / 64 + 1];
    index.positions_of_ascending(&codes, |position| {
        words[position as usize / 64] |= 1 << (position % 64);
    });
    let mut ascending: Vec<u32> = Vec::new();
    for (at, word) in words.iter().enumerate() {
        let mut bits = *word;
        while bits != 0 {
            ascending.push((at * 64) as u32 + bits.trailing_zeros());
            bits &= bits - 1;
        }
    }
    let mut positions = Bitmap::new();
    positions.add_many(&ascending);
    Some(SuggestSet::new(positions, values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn key(token_id: u64, segments_version: u64, overlay_version: u64) -> SuggestSetKey {
        SuggestSetKey {
            token_id,
            column: "department".to_string(),
            segments_version,
            overlay_version,
        }
    }

    fn set(positions: &[u32]) -> Arc<SuggestSet> {
        let mut bitmap = Bitmap::new();
        bitmap.add_many(positions);
        Arc::new(SuggestSet::new(bitmap, 1_000))
    }

    /// **A set is served only for its exact key** — the rule that keeps a suppression from being
    /// answered around. Neither a moved overlay nor a moved generation may reach the entry.
    #[test]
    fn a_set_is_not_served_under_a_moved_generation_or_overlay() {
        let sets = SuggestSets::default();
        sets.finish(key(1, 7, 3), set(&[4, 9]));

        assert!(sets.get(&key(1, 7, 3), 1_000).is_some());
        assert!(sets.get(&key(1, 7, 4), 1_000).is_none(), "a deny landed");
        assert!(sets.get(&key(1, 8, 3), 1_000).is_none(), "a flush landed");
        assert!(sets.get(&key(2, 7, 3), 1_000).is_none(), "another principal");
    }

    /// One sweep in flight per key (§6.3 rule 2), and a claim is released either way.
    #[test]
    fn one_sweep_is_claimed_at_a_time_and_released_either_way() {
        let sets = SuggestSets::default();
        assert!(sets.claim(&key(1, 7, 3)));
        assert!(!sets.claim(&key(1, 7, 3)), "already in flight");
        assert!(sets.claim(&key(1, 7, 4)), "a different key is its own sweep");

        sets.abandon(&key(1, 7, 3));
        assert!(sets.claim(&key(1, 7, 3)), "an abandoned claim frees the key");

        sets.finish(key(1, 7, 3), set(&[1]));
        assert!(!sets.claim(&key(1, 7, 3)), "a held set needs no sweep");
    }

    /// A finished sweep drops the same session-and-column sets taken under older versions: nothing
    /// will ask for them again, and they are the residency a long session would otherwise leak.
    #[test]
    fn a_finished_sweep_drops_its_own_supersessions() {
        let sets = SuggestSets::default();
        sets.finish(key(1, 7, 3), set(&[1, 2, 3]));
        sets.finish(key(1, 7, 4), set(&[1, 2]));
        assert_eq!(sets.stats().entries, 1);
        assert!(sets.get(&key(1, 7, 4), 1_000).is_some());

        // Another principal's set at the old versions is untouched.
        sets.finish(key(2, 7, 3), set(&[5]));
        assert_eq!(sets.stats().entries, 2);
    }

    #[test]
    fn the_bound_evicts_least_recently_used_first() {
        let sets = SuggestSets::new(1024);
        for token in 1..=8u64 {
            sets.finish(key(token, 1, 1), set(&[token as u32]));
        }
        let stats = sets.stats();
        assert!(stats.resident_bytes <= 1024, "{stats:?}");
        assert!(stats.evictions > 0, "{stats:?}");
        assert!(
            sets.get(&key(8, 1, 1), 1_000).is_some(),
            "the newest entry survives"
        );
    }

    /// The sweep over a real value column: the codes the candidate's entities carry become the
    /// dense positions of the values those codes stand for, and nothing else does.
    ///
    /// **Mutations this kills:** a set built over codes rather than positions; a sweep that ignored
    /// the candidate; a sweep that admitted a code the vocabulary does not hold (here the absent
    /// sentinel, code 0, which every valueless entity carries and which is bound to no key).
    #[test]
    fn the_sweep_turns_the_candidates_codes_into_dense_positions() {
        let dir = tempfile::TempDir::new().unwrap();
        let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
        // Four values, codes scattered over the `u32` width as a real vocabulary's are (§3.4), so
        // position and code cannot be confused for one another.
        let values: Vec<crate::suggest::SuggestValue> = ["alpha", "beta", "gamma", "delta"]
            .iter()
            .enumerate()
            .map(|(at, key)| crate::suggest::SuggestValue {
                key: (*key).to_string(),
                title: None,
                code: 900_000_000 - (at as u32) * 137_000_000,
            })
            .collect();
        let mut in_key_order = values.clone();
        in_key_order.sort_by(|a, b| a.key.cmp(&b.key));
        let index = crate::suggest::SuggestIndex::build(dir.path(), 0, &in_key_order, &pool)
            .expect("the index builds");

        // Entity e carries value e % 4, except entity 4, which carries the absent sentinel.
        let codes: Vec<u32> = (0..8u32)
            .map(|e| {
                if e == 4 {
                    tessera_store::vocabulary::ABSENT_CODE
                } else {
                    in_key_order[(e % 4) as usize].code
                }
            })
            .collect();
        let column = tessera_filter::ValueColumn::universal(tessera_filter::Codes::U32(
            codes.into(),
        ));

        let mut candidate = Bitmap::new();
        candidate.add_many(&[1, 4, 6]);
        let set = sweep(std::iter::once(&column), &candidate, &index).expect("a category sweeps");
        // Entities 1 and 6 carry positions 1 and 2; entity 4 carries the sentinel, which is bound
        // to no key and so contributes nothing.
        assert!(set.contains(1));
        assert!(set.contains(2));
        assert!(!set.contains(0));
        assert!(!set.contains(3));
        assert_eq!(set.visible_values(), 2);
        assert_eq!(set.values(), 4);
    }

    /// **Rule 4: a column with nothing to sweep never takes the set.** No layers at all is the
    /// blob-resident `derived` category — postings and no entity-space values — and a column of
    /// another family is the fail-closed arm: a value set built from an `f64` column would be a set
    /// of sentinels that reads like an answer.
    #[test]
    fn a_column_with_nothing_to_sweep_takes_no_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
        let values = vec![crate::suggest::SuggestValue {
            key: "alpha".to_string(),
            title: None,
            code: 7,
        }];
        let index = crate::suggest::SuggestIndex::build(dir.path(), 0, &values, &pool)
            .expect("the index builds");
        let candidate = Bitmap::from_range(0..4);

        assert!(sweep(std::iter::empty(), &candidate, &index).is_none());

        let numbers = tessera_filter::ValueColumn::universal(tessera_filter::Codes::F64(
            vec![1.0f64, 2.0, 3.0, 4.0].into(),
        ));
        assert!(sweep(std::iter::once(&numbers), &candidate, &index).is_none());
    }

    /// **A set swept against a superseded index is rejected *and dropped*.** Rejecting without
    /// removing would leave [`SuggestSets::claim`] finding a set for the key and refusing to sweep,
    /// so the pair would stay on the probe route until the key rotated or eviction reached it —
    /// which for a session typing steadily into one column is neither. The same request that finds
    /// it stale must be the one that starts its replacement.
    #[test]
    fn a_set_swept_against_a_shorter_index_is_rejected_and_dropped() {
        let sets = SuggestSets::default();
        sets.finish(key(1, 7, 3), set(&[4, 9]));
        assert!(!sets.claim(&key(1, 7, 3)), "a held set needs no sweep");

        // The vocabulary grew: a rebuild folded the side map into the base, moving positions, and
        // moved neither version in the key.
        assert!(sets.get(&key(1, 7, 3), 1_001).is_none());
        let stats = sets.stats();
        assert_eq!(stats.discarded, 1, "{stats:?}");
        assert_eq!(stats.entries, 0, "the entry is gone, not merely unserved");
        assert_eq!(stats.resident_bytes, 0, "{stats:?}");
        assert!(sets.claim(&key(1, 7, 3)), "the pair may sweep again at once");
    }

    #[test]
    fn pruning_a_token_frees_its_bytes() {
        let sets = SuggestSets::default();
        sets.finish(key(1, 7, 3), set(&[1, 2]));
        sets.finish(key(2, 7, 3), set(&[3]));
        sets.prune_token(1);
        assert_eq!(sets.stats().entries, 1);
        assert!(sets.get(&key(2, 7, 3), 1_000).is_some());
    }
}
