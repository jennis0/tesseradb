//! D-G's slot-state single-flight cache, fallible form (lifecycle §3.3), with Task 5's byte bound.
//!
//! Same shape as `tessera-engine::single_flight::SingleFlightCache` (commit d9baada, Task 1 of the
//! concurrency workstream; Task 5 of Phase 2 stage 2.1 for the bound): a slot per key is either
//! [`Slot::Building`] or [`Slot::Ready`], the map's mutex is held only for the O(1) transition
//! between those states — never across the build itself — and a concurrent arrival on the same key
//! does not wait for an in-flight build: it gets [`SingleFlightError::Building`] immediately (D-G's
//! non-blocking-waiters rule: a parked waiter would hold the server's admission budget while
//! burning zero CPU).
//!
//! **Duplicated rather than reused**, deliberately: `tessera-authz` sits *below* `tessera-engine`
//! in the crate graph (engine depends on authz, per `crates/tessera-engine/Cargo.toml`), and
//! `scripts/check-layers.sh` enforces that direction, so this crate cannot take a dependency on
//! `tessera-engine` to reuse its module. The two use sites also need different signatures: the
//! row-projection build engine's cache wraps is infallible, while the fragment build this cache
//! wraps is `io::Result` — a shared module would need exactly the generalisation
//! ([`get_or_try_build`](SingleFlightCache::get_or_try_build)) this module carries anyway.
//!
//! **The duplication is now five rules deep, so both copies carry their own tests.** Byte bound,
//! LRU, `CacheWeight`, the build sequence number and never-evict-`Building` can each be right in
//! one crate and wrong in the other, and a test written once covers only the crate it lives in. See
//! `tessera-engine/src/single_flight.rs`'s module doc for the fuller design rationale — the F4
//! measurement this pattern answers, and the four eviction rules, argued once there and referred to
//! by number here.
//!
//! **That claim was made before it was true, which is why the table below exists.** The round-1
//! review reverted [`RemoveUnlessReady`]'s `seq` comparison to a key-only match and
//! `cargo test -p tessera-authz` stayed green, while the identical mutation in the engine killed
//! its named test. Three of this module's tests were simply missing. The consequence is worse here
//! than there: [`crate::fragment::FragmentCache::evict`] is `pub` (stage 2.4's conformance command
//! is its intended caller) and this build is fallible, so the mid-build removal the guard has to
//! survive needs no panic at all — an `io::Error` from the postings read suffices.
//!
//! # Which test covers which rule in which crate
//!
//! The engine twin carries the same table. A missing twin shows up by reading it.
//!
//! | Rule / property | this crate | `tessera-engine` |
//! |---|---|---|
//! | 1 — `Building` is never a victim | `a_building_slot_is_never_evicted` | same name |
//! | 1 — the eviction loop terminates regardless | `an_undersized_bound_does_not_livelock` | same name |
//! | 2 — the publish compares `seq` | `an_evict_during_a_build_is_not_undone_by_the_publish` | `a_prune_during_a_build_is_not_undone_by_the_publish` |
//! | 2 — the unwind/`Err` guard compares `seq` | `a_failed_build_does_not_delete_a_later_builders_slot` | `an_unwinding_build_does_not_delete_a_later_builders_slot` |
//! | 3 — the builder receives what it built | `an_entry_larger_than_the_bound_is_served_but_not_retained`, `an_undersized_bound_does_not_livelock` | same two names |
//! | 4 — evicted values drop outside the lock | `evicted_arcs_are_dropped_outside_the_lock` | same name |
//! | 4 — on the bulk/explicit-removal path too | `evicted_arcs_from_evict_are_dropped_outside_the_lock` | `pruned_arcs_are_dropped_outside_the_lock` |
//! | the floor turns a byte bound into an entry bound | `tiny_entries_are_charged_the_floor`, the `const _` below | same name, the `const _` there |
//! | the counted choke point | `a_hit_takes_one_lock_and_a_miss_takes_two` | `a_hit_takes_exactly_one_lock`, `a_miss_takes_exactly_two_locks` |
//! | LRU order, not insertion order | `eviction_takes_the_least_recently_used` | same name |
//! | `young_evictions` is the thrash alarm | `young_evictions_counts_only_never_reused_entries` | same name |
//! | single-flight, non-blocking waiters, panic safety | `a_ready_hit_never_calls_build_again`, `concurrent_miss_…`, `distinct_keys_…`, `a_panicking_build_…` | same four names |
//!
//! **Three deliberate asymmetries**, listed so they are not read as gaps: single-key
//! [`SingleFlightCache::evict`] (`evict_removes_exactly_one_key`) exists only here, bulk
//! `retain_keys` only there, and the `Err` form of a failed build
//! (`a_failed_build_leaves_the_key_absent_so_a_retry_rebuilds`) only here, because the engine's
//! build is infallible.
//!
//! **One naming rule for the pair:** the same mechanism carries the same name in both copies, so a
//! fix ported across does not have to be re-derived. The guard's disarm flag was `ready` here and
//! `disarmed` there — opposite polarity for one mechanism, i.e. a ported fix had to be inverted by
//! hand — and is now `disarmed` in both.

use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustc_hash::FxHashMap;

/// What one cached value costs the byte bound. See the engine twin's trait of the same name.
pub(crate) trait CacheWeight {
    /// This value's contribution to the byte bound, **before** the per-entry floor. Computed once
    /// per successful build, outside the lock.
    fn cache_weight_bytes(&self) -> u64;
}

/// The minimum a cached entry is charged — **modelled, not measured**; see the engine twin's
/// constant for the inventory it is built from and for why it is not derived from `size_of`.
///
/// A byte bound alone does not bound entry count, and this is what makes it do so. The engine
/// twin's exposure is session rotation; this cache's is different in shape but not in kind — see
/// `FragmentCache`'s `key_memo` bound, which closes the sibling path.
pub(crate) const PER_ENTRY_FLOOR_BYTES: u64 = 512;

/// One key's state. There is deliberately no third, "failed" state: a failed (`Err`-returning or
/// panicking) build must remove the entry outright rather than cache anything for it, so the next
/// arrival retries — caching a failure would be a permanent fail-closed wedge for that credential
/// (I13a).
enum Slot<K, V> {
    /// A build is in flight. `seq` identifies *which* build — see the engine twin's rule 2, and
    /// [`RemoveUnlessReady`] below.
    Building { seq: u64 },
    Ready {
        value: Arc<V>,
        /// The map's own key, so a touch needs no `K: Clone` on the hot path (a 32-byte canonical
        /// key here, a `String`-bearing struct in the engine) and exactly one hash lookup: the new
        /// tick is stamped inside the same `get_mut` arm that reads the value. Documented in both
        /// copies — it was bare here and argued there, which is how the two drift.
        key: Arc<K>,
        /// `max(weight, PER_ENTRY_FLOOR_BYTES)` — what this entry contributes to `bytes`. Stored
        /// rather than recomputed, so a removal can never disagree with the insertion about how
        /// much to give back.
        charged: u64,
        /// This entry's key in [`Slots::recency`]. Ticks are unique and monotonic for the life of
        /// the cache, which is what makes this a bijection with the index.
        tick: u64,
        /// Hits since publication, `0` at the moment it is published. An eviction at `0` is an
        /// entry that was never reused — the thrash signature [`CacheStats::young_evictions`]
        /// reports.
        uses: u32,
    },
}

/// A losing (or unlucky) arrival's outcome.
#[derive(Debug)]
pub(crate) enum SingleFlightError<E> {
    /// Another caller is already building this key right now; this call did not wait for it (D-G).
    /// The caller decides what that means — `FragmentCache::get_or_build` surfaces it as
    /// `FragmentCacheError::Building`, which `Engine::authorise` turns into
    /// `EngineError::FragmentBuilding`.
    Building,
    /// The build that *this* call ran failed. The entry has already been removed (never cached —
    /// I13a fail-closed) by the time this is returned, so the next arrival sees a plain miss.
    Build(E),
}

/// Operator-facing cache gauges — the authz twin of `tessera_engine`'s `CacheStats`. Every field is
/// read from an atomic without taking the slot lock, so [`Self::slot_locks`] measures this type's
/// own locking rather than the caller's polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Slots currently held, `Building` and `Ready` both counted.
    pub entries: usize,
    /// **Charged** bytes resident: `Σ max(weight, PER_ENTRY_FLOOR_BYTES)` over `Ready` slots.
    pub bytes: u64,
    pub bound_bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub building_refusals: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    /// Evictions of entries never reused — the thrash signature. See the engine twin.
    pub young_evictions: u64,
    pub oversized_admissions: u64,
    /// Every acquisition of the slot mutex, from anywhere in this module. See
    /// [`SingleFlightCache::lock_slots`]; `is_locked_now`'s `try_lock` is deliberately not counted.
    pub slot_locks: u64,
}

/// The guarded state. One struct so the map, the recency index and the byte accounting cannot be
/// updated apart — see [`Slots::remove`].
struct Slots<K, V> {
    map: FxHashMap<Arc<K>, Slot<K, V>>,
    /// `Ready` entries only, oldest-use first. **`Building` slots are deliberately absent** (engine
    /// twin, rule 1): an eviction pass that had to pop-then-skip one would leave a slot holding a
    /// tick with no index entry, permanently unevictable while still charged — the bound stops
    /// holding while the counters say it holds.
    recency: BTreeMap<u64, Arc<K>>,
    next_tick: u64,
    next_seq: u64,
    bytes: u64,
}

impl<K: Eq + Hash, V> Slots<K, V> {
    /// **The one place a slot is removed** — map, recency index and `bytes` updated together.
    /// Returns the value rather than dropping it, so the caller drops it after releasing the lock
    /// (engine twin, rule 4). Not the only place all three are *written*: [`SingleFlightCache::
    /// publish`] does the insertion side by hand. Removal is the direction with four callers.
    ///
    /// `#[must_use]` is rule 4's only compile-time enforcement, in both copies. The lapse it
    /// refuses is `for key in doomed { slots.remove(key); }` — the natural shape for a bulk remover
    /// someone adds later — which drops the values in place, inside the critical section. The two
    /// callers that genuinely discard bind the result and assert on it; both remove a `Building`
    /// slot, which carries no value at all.
    #[must_use = "rule 4: the removed value must be carried out of the critical section and dropped \
                  after the lock is released, never dropped in place"]
    fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        match self.map.remove(key) {
            None | Some(Slot::Building { .. }) => None,
            Some(Slot::Ready {
                value,
                charged,
                tick,
                ..
            }) => {
                self.recency.remove(&tick);
                self.bytes -= charged;
                Some(value)
            }
        }
    }

    /// The invariant every locked section leaves true: the index holds exactly the `Ready` slots,
    /// and `bytes` is exactly their charged sum.
    ///
    /// `debug_assertions`-shaped rather than a release check because it is O(n), and it runs in
    /// every test in this file. **Read the limit as well as the check**: a shipped build has no
    /// assertion here at all, so nothing about eviction may be allowed to *depend* on the bijection
    /// holding — which is why [`SingleFlightCache::publish`]'s loop pops the recency entry rather
    /// than selecting it and trusting [`Slots::remove`] to unlink it. Argued in both copies; it was
    /// silent here and argued in the engine, which is how the two drift.
    fn check(&self) {
        #[cfg(debug_assertions)]
        {
            let ready = self
                .map
                .values()
                .filter(|slot| matches!(slot, Slot::Ready { .. }))
                .count();
            debug_assert_eq!(
                ready,
                self.recency.len(),
                "recency index desynchronised from the slot map"
            );
            let charged: u64 = self
                .map
                .values()
                .map(|slot| match slot {
                    Slot::Ready { charged, .. } => *charged,
                    Slot::Building { .. } => 0,
                })
                .sum();
            debug_assert_eq!(charged, self.bytes, "byte accounting desynchronised");
        }
    }
}

/// A map of independently single-flighted slots, bounded in bytes. See the module doc.
pub(crate) struct SingleFlightCache<K, V> {
    slots: Mutex<Slots<K, V>>,
    bound_bytes: AtomicU64,
    entries: AtomicUsize,
    bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    building_refusals: AtomicU64,
    evictions: AtomicU64,
    evicted_bytes: AtomicU64,
    young_evictions: AtomicU64,
    oversized_admissions: AtomicU64,
    slot_locks: AtomicU64,
}

#[derive(Default)]
struct EvictionTally {
    count: u64,
    bytes: u64,
    young: u64,
}

impl<K: Eq + Hash + Clone, V: CacheWeight> SingleFlightCache<K, V> {
    /// `u64::MAX` means "no bound" — the pre-Task-5 behaviour, and what every construction site
    /// that does not set one explicitly gets.
    pub(crate) fn new(bound_bytes: u64) -> Self {
        SingleFlightCache {
            slots: Mutex::new(Slots {
                map: FxHashMap::default(),
                recency: BTreeMap::new(),
                next_tick: 0,
                next_seq: 0,
                bytes: 0,
            }),
            bound_bytes: AtomicU64::new(bound_bytes),
            entries: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            building_refusals: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            evicted_bytes: AtomicU64::new(0),
            young_evictions: AtomicU64::new(0),
            oversized_admissions: AtomicU64::new(0),
            slot_locks: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_bound_bytes(&self, bound_bytes: u64) {
        self.bound_bytes.store(bound_bytes, Ordering::Relaxed);
    }

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic (fail-closed tests
    /// confirm a failed build leaves this at the count it started at, never wedged), not a capacity
    /// bound. `FragmentCache::slot_count` re-exports this publicly.
    pub(crate) fn len(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            bound_bytes: self.bound_bytes.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            building_refusals: self.building_refusals.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            evicted_bytes: self.evicted_bytes.load(Ordering::Relaxed),
            young_evictions: self.young_evictions.load(Ordering::Relaxed),
            oversized_admissions: self.oversized_admissions.load(Ordering::Relaxed),
            slot_locks: self.slot_locks.load(Ordering::Relaxed),
        }
    }

    /// Remove one key, if present. Returns whether anything was removed.
    ///
    /// The in-memory tier only — see `FragmentCache::evict`, which is the caller and which carries
    /// the argument about the `.frag` sidecar being untouched.
    /// A mapped fragment can be hundreds of megabytes, so the value leaves the critical section
    /// unfreed and drops below (engine twin, rule 4) — `evicted_arcs_from_evict_are_dropped_
    /// outside_the_lock` asserts it rather than this comment claiming it.
    pub(crate) fn evict(&self, key: &K) -> bool {
        // Bound outside the block so it outlives the guard, which is the whole of rule 4 here.
        let dead;
        let removed = {
            let mut slots = self.lock_slots();
            let present = slots.map.contains_key(key);
            dead = slots.remove(key);
            self.entries.store(slots.map.len(), Ordering::Relaxed);
            self.bytes.store(slots.bytes, Ordering::Relaxed);
            slots.check();
            present
        };
        drop(dead); // outside the lock (engine twin, rule 4)
        removed
    }

    /// Look up `key`. A hit clones the `Arc`, touches the recency order and returns without calling
    /// `build` at all — this is the in-memory cache half of D-G. A miss makes this call the
    /// builder: publish `Building`, drop the lock, run `build()` outside it, re-lock, evict to fit,
    /// publish `Ready` on `Ok`. A *different* concurrent miss on the same key observed while this
    /// is in flight gets `Err(SingleFlightError::Building)` immediately. A re-entrant lookup during
    /// a build (the same thread calling back in from inside its own `build`) sees the same
    /// `Building` state and errors rather than deadlocking, for the same reason: the map lock is
    /// never held across `build`.
    ///
    /// **Fail-closed (I13a).** If `build` returns `Err` or unwinds, [`RemoveUnlessReady`] removes
    /// *this build's* `Building` entry — identified by its sequence number — before this call
    /// returns or the unwind propagates, so the key is left absent, never wedged at `Building` and
    /// never a cached `Err`.
    ///
    /// **The builder always receives what it built** (engine twin, rule 3): a value that does not
    /// fit the bound is returned to its caller and simply not retained.
    pub(crate) fn get_or_try_build<E>(
        &self,
        key: K,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<Arc<V>, SingleFlightError<E>> {
        // Declared before any guard so it drops after one — engine twin, rule 4.
        let mut dead: Vec<Arc<V>> = Vec::new();

        let (owned_key, seq) = {
            let mut slots = self.lock_slots();
            // Read before the `get_mut` borrow opens, so the whole touch happens inside the one
            // lookup — the engine twin's `Slot::Ready::key` doc carries the argument. A tick read
            // and not used on the miss branch costs nothing: only uniqueness matters.
            let new_tick = slots.next_tick;
            // `get_mut` by reference, never `entry(key.clone())`: with `Arc<K>` keys the entry API
            // would allocate an `Arc` on every call including a warm hit, which is the hot path.
            let hit = match slots.map.get_mut(&key) {
                None => None,
                Some(Slot::Building { .. }) => {
                    self.building_refusals.fetch_add(1, Ordering::Relaxed);
                    return Err(SingleFlightError::Building);
                }
                Some(Slot::Ready {
                    value,
                    key: slot_key,
                    tick,
                    uses,
                    ..
                }) => {
                    *uses = uses.saturating_add(1);
                    let old_tick = std::mem::replace(tick, new_tick);
                    Some((Arc::clone(value), Arc::clone(slot_key), old_tick))
                }
            };
            if let Some((value, slot_key, old_tick)) = hit {
                slots.next_tick += 1;
                slots.recency.remove(&old_tick);
                slots.recency.insert(new_tick, slot_key);
                slots.check();
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(value);
            }
            let owned_key = Arc::new(key.clone());
            let seq = slots.next_seq;
            slots.next_seq += 1;
            slots
                .map
                .insert(Arc::clone(&owned_key), Slot::Building { seq });
            self.entries.store(slots.map.len(), Ordering::Relaxed);
            slots.check();
            (owned_key, seq)
        };
        self.misses.fetch_add(1, Ordering::Relaxed);

        // Armed for the whole build; disarmed only after the publish below. Both an `Err` return
        // and an unwinding `build` leave this build's key absent rather than stuck at `Building` or
        // wrongly `Ready` — one mechanism covering two failure exits, which is why the sequence
        // check lives in the guard rather than only at the publish.
        let mut guard = RemoveUnlessReady {
            cache: self,
            key: Arc::clone(&owned_key),
            seq,
            disarmed: false,
        };

        let built = match build() {
            Ok(v) => v,
            Err(e) => return Err(SingleFlightError::Build(e)),
        };
        let value = Arc::new(built);

        // Outside the lock, deliberately — see the engine twin.
        let charged = value.cache_weight_bytes().max(PER_ENTRY_FLOOR_BYTES);
        let bound = self.bound_bytes.load(Ordering::Relaxed);

        let tally = {
            let mut slots = self.lock_slots();
            let tally = self.publish(
                &mut slots, &owned_key, seq, &value, charged, bound, &mut dead,
            );
            self.entries.store(slots.map.len(), Ordering::Relaxed);
            self.bytes.store(slots.bytes, Ordering::Relaxed);
            slots.check();
            tally
        };
        guard.disarmed = true;

        self.evictions.fetch_add(tally.count, Ordering::Relaxed);
        self.evicted_bytes.fetch_add(tally.bytes, Ordering::Relaxed);
        self.young_evictions
            .fetch_add(tally.young, Ordering::Relaxed);

        // `dead` drops here, with the lock released.
        Ok(value)
    }

    /// The publish half of a miss. Runs with the lock held. See the engine twin's `publish` for the
    /// three outcomes and why the oversized arm must *remove* the `Building` slot rather than leave
    /// it (a `Building` slot with no builder is a permanent fail-closed wedge for that credential —
    /// I13a, and the precise thing this module's two-state `Slot` exists to prevent).
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        slots: &mut Slots<K, V>,
        key: &Arc<K>,
        seq: u64,
        value: &Arc<V>,
        charged: u64,
        bound: u64,
        dead: &mut Vec<Arc<V>>,
    ) -> EvictionTally {
        match slots.map.get(&**key) {
            Some(Slot::Building { seq: found }) if *found == seq => {}
            _ => return EvictionTally::default(),
        }

        if charged > bound {
            // A `Building` slot: no value, so nothing to carry out of the critical section.
            let no_value = slots.remove(key);
            debug_assert!(no_value.is_none(), "a Building slot carries no value");
            self.oversized_admissions.fetch_add(1, Ordering::Relaxed);
            return EvictionTally::default();
        }

        // **`pop_first`, not `values().next()`** — every iteration shrinks `recency` by one
        // whatever the map says about that key, so the loop terminates unconditionally rather than
        // by the bijection holding. `Slots::check` is `debug_assertions`-only, so in a `--release`
        // build the alternative is a spin under the request-path mutex. Engine twin, rule 1.
        let mut tally = EvictionTally::default();
        while slots.bytes + charged > bound {
            let Some((_, victim)) = slots.recency.pop_first() else {
                break;
            };
            let (young, freed) = match slots.map.get(&*victim) {
                Some(Slot::Ready { uses, charged, .. }) => (*uses == 0, *charged),
                _ => (false, 0),
            };
            // Counted only when a value actually came back: a `None` here is a broken bijection,
            // and counting it would leave the operator's gauges permanently ahead of the bytes
            // actually freed — the reading that says the bound is holding while it is not.
            match slots.remove(&victim) {
                Some(evicted) => {
                    dead.push(evicted);
                    tally.count += 1;
                    tally.bytes += freed;
                    tally.young += u64::from(young);
                }
                None => debug_assert!(
                    false,
                    "the recency index named a key the slot map does not hold as Ready"
                ),
            }
        }

        let tick = slots.next_tick;
        slots.next_tick += 1;
        slots.recency.insert(tick, Arc::clone(key));
        slots.map.insert(
            Arc::clone(key),
            Slot::Ready {
                value: Arc::clone(value),
                key: Arc::clone(key),
                charged,
                tick,
                uses: 0,
            },
        );
        slots.bytes += charged;
        tally
    }

    /// **The only place this module takes the lock** — see the engine twin for the argument.
    /// `is_locked_now`'s `try_lock` is the one deliberate, uncounted exception.
    ///
    /// A poisoned mutex is recovered from rather than propagated. The discharge is restated rather
    /// than inherited, because Task 5 made the guarded value richer: [`Slots`] now holds a map, a
    /// recency index and a byte counter, and the claim is that no critical section here can panic
    /// *between* two of their updates. [`Slots::remove`] is the only function that touches all
    /// three, and it performs no allocation and calls no user code between them; the publish path's
    /// inserts likewise cannot unwind partway. So a poisoning leaves the three consistent, and
    /// refusing every subsequent authorise because one unrelated thread unwound would fail closed
    /// on availability while buying no safety.
    fn lock_slots(&self) -> MutexGuard<'_, Slots<K, V>> {
        self.slot_locks.fetch_add(1, Ordering::Relaxed);
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the slot lock is held at this instant — the probe that makes "evicted `Arc`s drop
    /// outside the lock" assertable. Same three caveats as the engine twin: single-threaded tests
    /// only, a poisoned mutex reads as locked, and it is deliberately not counted in
    /// [`CacheStats::slot_locks`].
    #[cfg(test)]
    pub(crate) fn is_locked_now(&self) -> bool {
        self.slots.try_lock().is_err()
    }
}

/// Removes *this build's* `Building` slot unless it published. Covers both early exits — the `Err`
/// return and the unwind — with one mechanism.
///
/// **Compares `seq`, not just state**, for the reason the engine twin's guard gives: once a pruner
/// or an `evict` can remove a slot mid-build, a guard that removed by key alone would delete a
/// *later* builder's slot. Unreachable before Task 5 gave this cache [`SingleFlightCache::evict`];
/// hardened in the same change that makes it reachable.
struct RemoveUnlessReady<'a, K: Eq + Hash + Clone, V: CacheWeight> {
    cache: &'a SingleFlightCache<K, V>,
    key: Arc<K>,
    seq: u64,
    /// Set once the publish has decided this slot's fate. **Named `disarmed`, matching the engine
    /// twin's guard** — it was `ready` here, the opposite polarity for the same mechanism, so a fix
    /// ported between the two copies had to be inverted by hand.
    disarmed: bool,
}

impl<K: Eq + Hash + Clone, V: CacheWeight> Drop for RemoveUnlessReady<'_, K, V> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // This guard's own `drop` can run while a panic is already unwinding through it, so a
        // poisoned mutex must not be treated as a second panic here — that would abort the process
        // instead of completing the unwind. `lock_slots` recovers, and its doc discharges why.
        let mut slots = self.cache.lock_slots();
        if matches!(slots.map.get(&*self.key), Some(Slot::Building { seq }) if *seq == self.seq) {
            // A `Building` slot carries no value and no charged bytes, so this frees nothing that
            // could convoy. If this guard ever becomes able to remove a `Ready` slot, the value
            // must be carried out of the critical section like every other path; the binding and
            // the assertion are what make that change loud rather than silent.
            let no_value = slots.remove(&self.key);
            debug_assert!(no_value.is_none(), "a Building slot carries no value");
            self.cache.entries.store(slots.map.len(), Ordering::Relaxed);
            slots.check();
        }
    }
}

/// The floor must never become an *under*-charge as an entry's own bookkeeping grows. Checks only
/// the parts `size_of` can see; the opaque C-side allocation is precisely why the constant is not
/// derived from it. See [`PER_ENTRY_FLOOR_BYTES`] and the engine twin's identical assertion — this
/// one was missing, which is the drift the module doc's table exists to make visible.
const _: () = {
    assert!(
        std::mem::size_of::<u64>() * 4 + std::mem::size_of::<usize>() * 4
            <= PER_ENTRY_FLOOR_BYTES as usize
    );
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

    /// A test value of a chosen weight — the eviction cases need entries whose sizes they control.
    struct Weighed(u32, u64);

    impl CacheWeight for Weighed {
        fn cache_weight_bytes(&self) -> u64 {
            self.1
        }
    }

    const BIG: u64 = 10_000;

    fn unbounded() -> SingleFlightCache<u32, Weighed> {
        SingleFlightCache::new(u64::MAX)
    }

    /// A hit must never call `build` — the closure panics if invoked, so any accidental rebuild on
    /// a warm key fails the test loudly rather than merely wasting work.
    #[test]
    fn a_ready_hit_never_calls_build_again() {
        let cache = unbounded();
        let first = cache
            .get_or_try_build(1, || Ok::<_, ()>(Weighed(42, BIG)))
            .unwrap();
        assert_eq!(first.0, 42);

        let second = cache
            .get_or_try_build(1, || -> Result<Weighed, ()> {
                panic!("must not rebuild a Ready key")
            })
            .unwrap();
        assert_eq!(second.0, 42);
        assert!(Arc::ptr_eq(&first, &second), "same Arc, not a fresh build");
    }

    /// D-G's core claim, reproduced deterministically: a concurrent arrival on the same key while a
    /// build is in flight gets `Building` immediately rather than blocking, and once the build
    /// publishes `Ready`, both the retried loser and a fresh arrival observe it without rebuilding.
    #[test]
    fn concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_try_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Ok::<_, ()>(Weighed(99, BIG))
            })
        });

        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let loser = cache.get_or_try_build(1, || -> Result<Weighed, ()> {
            panic!("a losing arrival must not build")
        });
        assert!(matches!(loser, Err(SingleFlightError::Building)));

        release_tx.send(()).unwrap();
        let built = builder.join().unwrap().unwrap();
        assert_eq!(built.0, 99);

        let retried = cache
            .get_or_try_build(1, || -> Result<Weighed, ()> {
                panic!("must not rebuild once Ready")
            })
            .unwrap();
        assert_eq!(retried.0, 99);
    }

    /// The map lock is held only for the O(1) transition, never for the build — key `1`'s build
    /// blocks while key `2`'s runs to completion on another thread.
    #[test]
    fn distinct_keys_never_contend_on_a_slow_build() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_try_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Ok::<_, ()>(Weighed(1, BIG))
            })
        });

        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let other = cache
            .get_or_try_build(2, || Ok::<_, ()>(Weighed(2, BIG)))
            .unwrap();
        assert_eq!(other.0, 2);

        release_tx.send(()).unwrap();
        assert_eq!(slow.join().unwrap().unwrap().0, 1);
    }

    /// I13a: a build returning `Err` must never leave a permanent `Building` wedge, and the error
    /// must never be cached — the entry is absent afterwards, so the very next call retries cleanly
    /// (and can succeed, unlike a cached failure, which would be a permanent fail-closed wedge).
    #[test]
    fn a_failed_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache = unbounded();

        let result = cache.get_or_try_build(7, || Err::<Weighed, _>("boom"));
        assert!(matches!(result, Err(SingleFlightError::Build("boom"))));
        assert_eq!(
            cache.len(),
            0,
            "a failed build must not leave a Building wedge, nor cache the Err (I13a)"
        );

        let rebuilt = cache
            .get_or_try_build(7, || Ok::<_, &str>(Weighed(7, BIG)))
            .unwrap();
        assert_eq!(rebuilt.0, 7);
        assert_eq!(cache.len(), 1);
    }

    /// I13a, panic form: same guarantee via unwinding rather than a returned `Err`.
    #[test]
    fn a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache = unbounded();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_try_build(7, || -> Result<Weighed, ()> { panic!("boom") })
        }));
        assert!(result.is_err(), "the panic must propagate to the caller");
        assert_eq!(
            cache.len(),
            0,
            "a panicked build must not leave a Building wedge (I13a)"
        );

        let rebuilt = cache
            .get_or_try_build(7, || Ok::<_, ()>(Weighed(7, BIG)))
            .unwrap();
        assert_eq!(rebuilt.0, 7);
        assert_eq!(cache.len(), 1);
    }

    /// The counted choke point, both paths. A warm hit is one acquisition (the LRU touch is inside
    /// it); a miss is two (publish `Building`, publish `Ready`, eviction inside the second).
    #[test]
    fn a_hit_takes_one_lock_and_a_miss_takes_two() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache
            .get_or_try_build(1, || Ok::<_, ()>(Weighed(1, BIG)))
            .unwrap();

        let before = cache.stats().slot_locks;
        cache
            .get_or_try_build(1, || -> Result<Weighed, ()> { panic!("warm") })
            .unwrap();
        assert_eq!(cache.stats().slot_locks - before, 1, "a warm hit: one");

        let before = cache.stats().slot_locks;
        cache
            .get_or_try_build(2, || Ok::<_, ()>(Weighed(2, BIG)))
            .unwrap();
        assert_eq!(cache.stats().slot_locks - before, 2, "a miss: two");
    }

    /// Rule 4, made assertable — the authz copy. A value whose `Drop` observes the lock state
    /// records a violation if it is dropped inside the critical section.
    #[test]
    fn evicted_arcs_are_dropped_outside_the_lock() {
        use std::cell::{Cell, RefCell};

        thread_local! {
            static PROBE: RefCell<Option<*const SingleFlightCache<u32, Tattle>>> =
                const { RefCell::new(None) };
            static VIOLATION: Cell<bool> = const { Cell::new(false) };
        }

        struct Tattle(u64);
        impl CacheWeight for Tattle {
            fn cache_weight_bytes(&self) -> u64 {
                self.0
            }
        }
        impl Drop for Tattle {
            fn drop(&mut self) {
                PROBE.with(|probe| {
                    if let Some(cache) = *probe.borrow() {
                        // SAFETY: set and cleared inside the test body; the cache outlives every
                        // value it holds.
                        if unsafe { &*cache }.is_locked_now() {
                            VIOLATION.with(|v| v.set(true));
                        }
                    }
                });
            }
        }

        let cache = SingleFlightCache::<u32, Tattle>::new(BIG * 2);
        PROBE.with(|probe| *probe.borrow_mut() = Some(&cache as *const _));

        cache
            .get_or_try_build(1, || Ok::<_, ()>(Tattle(BIG)))
            .unwrap();
        cache
            .get_or_try_build(2, || Ok::<_, ()>(Tattle(BIG)))
            .unwrap();
        cache
            .get_or_try_build(3, || Ok::<_, ()>(Tattle(BIG)))
            .unwrap();

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(cache.stats().evictions >= 1, "the test must have evicted");
        assert!(
            !VIOLATION.with(Cell::get),
            "an evicted value was dropped while the slot lock was held"
        );
    }

    /// Rule 1 — the authz copy. A `Building` slot is never chosen as an eviction victim.
    #[test]
    fn a_building_slot_is_never_evicted() {
        let cache = Arc::new(SingleFlightCache::<u32, Weighed>::new(BIG * 2));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_try_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Ok::<_, ()>(Weighed(1, BIG))
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        for key in 2..5u32 {
            cache
                .get_or_try_build(key, || Ok::<_, ()>(Weighed(key, BIG)))
                .unwrap();
        }

        let racer = cache.get_or_try_build(1, || -> Result<Weighed, ()> {
            panic!("key 1's Building slot was evicted")
        });
        assert!(matches!(racer, Err(SingleFlightError::Building)));

        release_tx.send(()).unwrap();
        assert_eq!(slow.join().unwrap().unwrap().0, 1);
    }

    /// The I13a wedge the oversized path would otherwise leave — the authz copy.
    #[test]
    fn an_entry_larger_than_the_bound_is_served_but_not_retained() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG);

        let built = cache
            .get_or_try_build(1, || Ok::<_, ()>(Weighed(5, BIG * 4)))
            .unwrap();
        assert_eq!(built.0, 5, "the builder receives what it built");
        assert_eq!(cache.stats().oversized_admissions, 1);
        assert_eq!(cache.stats().bytes, 0);
        assert_eq!(
            cache.len(),
            0,
            "the Building slot must be REMOVED: one with no builder is a permanent fail-closed \
             wedge for that credential (I13a)"
        );

        let again = cache.get_or_try_build(1, || Ok::<_, ()>(Weighed(6, BIG * 4)));
        assert!(matches!(&again, Ok(v) if v.0 == 6));
    }

    /// Eviction takes the least recently *used*, not the least recently inserted.
    #[test]
    fn eviction_takes_the_least_recently_used() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache
            .get_or_try_build(1, || Ok::<_, ()>(Weighed(1, BIG)))
            .unwrap();
        cache
            .get_or_try_build(2, || Ok::<_, ()>(Weighed(2, BIG)))
            .unwrap();
        cache
            .get_or_try_build(1, || -> Result<Weighed, ()> { panic!("warm") })
            .unwrap();
        cache
            .get_or_try_build(3, || Ok::<_, ()>(Weighed(3, BIG)))
            .unwrap();

        assert!(cache
            .get_or_try_build(1, || -> Result<Weighed, ()> {
                panic!("1 must be resident")
            })
            .is_ok());
        let rebuilt = std::cell::Cell::new(false);
        cache
            .get_or_try_build(2, || {
                rebuilt.set(true);
                Ok::<_, ()>(Weighed(2, BIG))
            })
            .unwrap();
        assert!(rebuilt.get(), "key 2 was the LRU victim");
    }

    /// `evict` removes one key and nothing else, in one acquisition.
    #[test]
    fn evict_removes_exactly_one_key() {
        let cache = unbounded();
        for key in 0..3u32 {
            cache
                .get_or_try_build(key, || Ok::<_, ()>(Weighed(key, BIG)))
                .unwrap();
        }

        let before = cache.stats().slot_locks;
        assert!(cache.evict(&1));
        assert_eq!(cache.stats().slot_locks - before, 1);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.stats().bytes, 2 * BIG);
        assert!(
            !cache.evict(&1),
            "a second evict of the same key removes nothing"
        );

        let rebuilt = std::cell::Cell::new(false);
        cache
            .get_or_try_build(1, || {
                rebuilt.set(true);
                Ok::<_, ()>(Weighed(1, BIG))
            })
            .unwrap();
        assert!(rebuilt.get(), "the evicted key must rebuild");
    }

    /// Rule 2 — an `evict` landing mid-build must not be undone by the publish that follows it.
    #[test]
    fn an_evict_during_a_build_is_not_undone_by_the_publish() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_try_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Ok::<_, ()>(Weighed(1, BIG))
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        cache.evict(&1);
        release_tx.send(()).unwrap();

        let built = builder.join().unwrap().unwrap();
        assert_eq!(built.0, 1, "the builder still receives its value");
        assert_eq!(
            cache.len(),
            0,
            "the publish must not resurrect a key the evict removed"
        );
    }

    /// **Rule 4 on the explicit-removal path** — the half `evicted_arcs_are_dropped_outside_the_
    /// lock` does not reach. [`SingleFlightCache::evict`] is `pub` through
    /// `FragmentCache::evict`, and the values it frees are *mapped fragments*: dropping one inside
    /// the critical section unmaps hundreds of megabytes while every caller of `authorise` waits.
    #[test]
    fn evicted_arcs_from_evict_are_dropped_outside_the_lock() {
        use std::cell::{Cell, RefCell};

        thread_local! {
            static PROBE: RefCell<Option<*const SingleFlightCache<u32, Tattle>>> =
                const { RefCell::new(None) };
            static VIOLATION: Cell<bool> = const { Cell::new(false) };
        }

        struct Tattle(u64);
        impl CacheWeight for Tattle {
            fn cache_weight_bytes(&self) -> u64 {
                self.0
            }
        }
        impl Drop for Tattle {
            fn drop(&mut self) {
                PROBE.with(|probe| {
                    if let Some(cache) = *probe.borrow() {
                        // SAFETY: set and cleared inside the test body; the cache outlives every
                        // value it holds.
                        if unsafe { &*cache }.is_locked_now() {
                            VIOLATION.with(|v| v.set(true));
                        }
                    }
                });
            }
        }

        let cache = SingleFlightCache::<u32, Tattle>::new(u64::MAX);
        PROBE.with(|probe| *probe.borrow_mut() = Some(&cache as *const _));

        // The returned `Arc` is dropped at the end of the statement, so the cache is the unique
        // owner and the evict below really does drop the value.
        cache
            .get_or_try_build(1, || Ok::<_, ()>(Tattle(BIG)))
            .unwrap();
        assert!(cache.evict(&1), "the entry was resident");

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(
            !VIOLATION.with(Cell::get),
            "an evicted value was dropped while the slot lock was held (rule 4)"
        );
    }

    /// **Rule 2, the sharper half — the twin the engine had and this crate did not.** A build that
    /// fails after its slot was evicted, and after a *later* builder claimed the key, must not
    /// delete that later builder's slot.
    ///
    /// Reverting [`RemoveUnlessReady`]'s `seq` comparison to a key-only match left the whole authz
    /// suite green before this existed (round-1 review, MX4). No panic is needed to reach the
    /// interleaving here, unlike in the engine: this cache's build is fallible, so an `io::Error`
    /// from the postings read — an ordinary runtime failure, not a bug — is enough.
    #[test]
    fn a_failed_build_does_not_delete_a_later_builders_slot() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let first_cache = Arc::clone(&cache);
        let first = thread::spawn(move || {
            first_cache.get_or_try_build(1, move || -> Result<Weighed, &'static str> {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Err("the fragment build failed")
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // `FragmentCache::evict` removes the first builder's slot; a second builder then claims the
        // same canonical key.
        assert!(cache.evict(&1), "the Building slot must be removed");
        let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
        let (second_release_tx, second_release_rx) = mpsc::channel::<()>();
        let second_cache = Arc::clone(&cache);
        let second = thread::spawn(move || {
            second_cache.get_or_try_build(1, move || {
                second_started_tx.send(()).unwrap();
                second_release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Ok::<_, &'static str>(Weighed(2, BIG))
            })
        });
        second_started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // Now let the first build fail. Its guard must leave the second builder's slot alone.
        release_tx.send(()).unwrap();
        assert!(matches!(
            first.join().unwrap(),
            Err(SingleFlightError::Build("the fragment build failed"))
        ));

        let racer = cache.get_or_try_build(1, || -> Result<Weighed, &'static str> {
            panic!("the second builder still owns this key")
        });
        assert!(
            matches!(racer, Err(SingleFlightError::Building)),
            "the failing first build deleted the second builder's slot"
        );

        second_release_tx.send(()).unwrap();
        assert_eq!(second.join().unwrap().unwrap().0, 2);
    }

    /// `young_evictions` counts only entries that were never reused — the thrash signature. An
    /// entry hit before being evicted is a healthy cold eviction and must not alarm. (The engine
    /// twin's test of the same name; this crate had none.)
    #[test]
    fn young_evictions_counts_only_never_reused_entries() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache
            .get_or_try_build(1, || Ok::<_, ()>(Weighed(1, BIG)))
            .unwrap();
        cache
            .get_or_try_build(2, || Ok::<_, ()>(Weighed(2, BIG)))
            .unwrap(); // never reused
                       // The touch must come AFTER key 2 is published, or key 1 is the older entry
                       // in recency order and it, not key 2, is the first victim.
        cache
            .get_or_try_build(1, || -> Result<Weighed, ()> { panic!("warm") })
            .unwrap();

        // Evicts key 2 — the LRU, never reused.
        cache
            .get_or_try_build(3, || Ok::<_, ()>(Weighed(3, BIG)))
            .unwrap();
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().young_evictions, 1, "key 2 was never reused");

        // Evicts key 1 — reused once before it went cold.
        cache
            .get_or_try_build(4, || Ok::<_, ()>(Weighed(4, BIG)))
            .unwrap();
        assert_eq!(cache.stats().evictions, 2);
        assert_eq!(
            cache.stats().young_evictions,
            1,
            "key 1 was reused before eviction — a healthy cold eviction, not thrash"
        );
    }

    /// **Forward progress under a bound below the working set** (engine twin's test of the same
    /// name; this crate had none). Five keys round-robin through a cache that holds two: every call
    /// returns its own value and none is ever refused — refusals come from *same-key* concurrency,
    /// which a round-robin never creates.
    ///
    /// It is also this pair's liveness test. Before the round-1 fixes, a rule-1 violation made the
    /// eviction loop re-select a victim it never removed, and in a `--release` build — where
    /// `Slots::check` compiles to nothing — this test *hung* under the request-path mutex instead
    /// of failing. `publish` now pops the recency entry, so termination does not depend on rule 1.
    #[test]
    fn an_undersized_bound_does_not_livelock() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        for round in 0..5 {
            for key in 0..5u32 {
                let got = cache
                    .get_or_try_build(key, || Ok::<_, ()>(Weighed(key, BIG)))
                    .unwrap_or_else(|_| panic!("round {round} key {key} was refused"));
                assert_eq!(got.0, key, "every caller receives its own value");
            }
        }
        assert_eq!(
            cache.stats().building_refusals,
            0,
            "a bound below the working set must cost rebuilds, never refusals"
        );
        assert!(cache.stats().bytes <= BIG * 2, "the bound held throughout");
    }

    /// Tiny entries are charged the floor, which is what turns a byte bound into an entry bound.
    #[test]
    fn tiny_entries_are_charged_the_floor() {
        let cache = SingleFlightCache::<u32, Weighed>::new(u64::MAX);
        for key in 0..10u32 {
            cache
                .get_or_try_build(key, || Ok::<_, ()>(Weighed(key, 1)))
                .unwrap();
        }
        assert_eq!(cache.stats().bytes, 10 * PER_ENTRY_FLOOR_BYTES);
    }
}
