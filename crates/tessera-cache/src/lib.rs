//! A slot-state single-flight cache with a byte bound, LRU eviction and pruning.
//!
//! A slot per key is either [`Slot::Building`] or [`Slot::Ready`]. The map's mutex is held only
//! for the O(1) transition between those states, never for the build itself, so distinct keys
//! never contend and one caller's multi-second build does not serialise the rest. A second arrival
//! on a key that is already building either waits for it within a budget
//! ([`SingleFlightCache::get_or_derive_waiting`]) or is refused immediately
//! ([`SingleFlightCache::get_or_derive`], [`SingleFlightCache::get_or_try_build`]).
//!
//! The rules the state machine keeps, each guarding a failure the natural implementation walks
//! into:
//!
//! 1. A `Building` slot is never an eviction victim. It weighs nothing, holds no value, and is
//!    absent from the recency index, so the index holds exactly the `Ready` slots and eviction
//!    takes its victim from the front of it unconditionally. The eviction loop pops that entry out
//!    of the index rather than selecting it and relying on the removal to unlink it, so every
//!    iteration shrinks the index by one and the loop terminates whatever the map says.
//! 2. A build publishes only into its own slot, identified by the `seq` stamped on `Building`.
//!    Both the publish and the guard that fires on `Err` or on unwind compare `seq`, so a removal
//!    landing mid-build is not undone by the publish that follows it, and a build that fails after
//!    a later builder claimed the key does not delete that builder's slot.
//! 3. The builder always receives what it built, retained or not. The value is returned from the
//!    build, never re-read from the map, so a bound below the working set costs rebuilds and never
//!    a refusal, and a value larger than the whole bound is served and simply not admitted.
//! 4. Evicted values are collected under the lock and dropped after it is released. Dropping a
//!    large bitmap or unmapping a fragment inside a critical section whose O(1) hold time is
//!    load-bearing convoys every waiting caller. Every removal funnels through [`Slots::remove`],
//!    which hands the value back rather than dropping it; its `#[must_use]` is what refuses the
//!    natural lapse of dropping it in place.
//! 5. Every exit from `Building` notifies that slot's waiters, and a waiter decides by re-reading
//!    the map rather than by trusting the wake. Three exits remove the slot and all three go
//!    through [`Slots::remove`]; the fourth overwrites it in [`SingleFlightCache::publish`]. Both
//!    notify with the map lock held, so no wake can land between a waiter deciding to sleep and
//!    sleeping. A waiter that finds anything other than a value falls back to building one itself,
//!    which is what a fresh arrival would do.
//!
//! Every entry is charged `max(weight, PER_ENTRY_FLOOR_BYTES)`. The floor is what turns a byte
//! bound into an entry bound: without it a caller inserting unbounded tiny entries trips no byte
//! bound while each entry costs hundreds of bytes of real memory.
//!
//! `bound_bytes` bounds the bytes resident in this map, not process memory: every in-flight caller
//! holds an `Arc<V>` for the duration of its request whether or not the map still contains it.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

/// What one cached value costs the byte bound.
///
/// A trait rather than a `fn(&V) -> u64` handed to the constructor: the size of a value is a
/// property of its type.
pub trait CacheWeight {
    /// This value's contribution to the byte bound, before the per-entry floor is applied.
    ///
    /// Must be cheap, and is computed once per successful build, outside the lock.
    fn cache_weight_bytes(&self) -> u64;
}

/// Whether the request a waiting caller belongs to has gone away.
///
/// A waiter polls this on a tick rather than being woken by it, so a cancellation releases within
/// [`WAIT_TICK`] rather than instantly.
pub trait Cancel {
    fn is_cancelled(&self) -> bool;
}

/// Stands in for a cancellation source at the entry points that never wait.
struct NeverCancelled;

impl Cancel for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// The minimum a cached entry is charged, whatever [`CacheWeight`] reports. Modelled, not
/// measured.
///
/// The inventory it covers, per entry: the `Arc<K>` allocation and its two refcounts, the key's
/// own heap buffer, a hashbrown slot, the recency node's amortised share, the `Arc<V>` allocation,
/// and a bitmap's opaque C-side container array, which `size_of` cannot see at all. That lands
/// near 200-260 bytes; 512 is the next round number above it, so the charge stays an
/// over-estimate. Over-charging evicts sooner than strictly necessary; under-charging is what
/// makes a bound stop bounding.
pub const PER_ENTRY_FLOOR_BYTES: u64 = 512;

/// One key's state.
///
/// There is deliberately no third, failed state: a build that fails or panics removes the entry
/// outright rather than caching anything for it, so the next arrival retries. A cached failure is
/// a permanent fail-closed wedge for that key.
enum Slot<K, V> {
    /// A build is in flight. `seq` identifies which build, so a publish or a guard can tell its
    /// own slot from a later builder's. Carries no value, is charged no bytes, and is absent from
    /// the recency index.
    ///
    /// `wake` is this slot's own condvar. It is notified by whichever write displaces this
    /// variant, and those are [`Slots::remove`] and the insert in [`SingleFlightCache::publish`].
    /// Per slot rather than per cache, so a publish wakes the callers waiting for that key and no
    /// others.
    Building { seq: u64, wake: Arc<Condvar> },
    Ready {
        value: Arc<V>,
        /// The map's own key. Re-inserting into the recency index needs an owned key, and holding
        /// it here makes that an `Arc` refcount bump rather than a clone of `K` on every hit. It
        /// is also what lets the whole touch happen inside one hash lookup.
        key: Arc<K>,
        /// `max(weight, PER_ENTRY_FLOOR_BYTES)`. Stored rather than recomputed, so a removal
        /// cannot disagree with the insertion about how much to give back.
        charged: u64,
        /// This entry's key in [`Slots::recency`]. Ticks are unique and monotonic for the life of
        /// the cache, which is what makes this a bijection with the index.
        tick: u64,
        /// Hits since publication. An eviction at `0` is an entry that was never reused, the
        /// thrash signature [`CacheStats::young_evictions`] reports.
        uses: u32,
    },
}

/// A losing arrival's outcome: another caller is already building this key and this call did not
/// wait for it. Carries nothing, because the caller only needs to know to retry.
#[derive(Debug)]
pub struct Building;

/// Why a waiting caller gave up. Both are refusals, and they are kept apart because a caller owes
/// its client different answers: a budget expiry is backpressure, a cancellation is the client's
/// own disconnect.
///
/// There is deliberately no "the build failed" variant. A build that panics, is oversized or is
/// removed leaves the waiter looking at a plain miss, which it then builds itself.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitEnded {
    /// The wait budget expired with no value to serve.
    Budget,
    /// The caller's [`Cancel`] source reported the request gone.
    Cancelled,
}

/// A fallible build's outcome.
#[derive(Debug)]
pub enum SingleFlightError<E> {
    /// Another caller is already building this key right now, and this call did not wait for it.
    Building,
    /// The build this call ran failed. The entry is already absent by the time this returns, so
    /// the next arrival sees a plain miss rather than a cached failure.
    Build(E),
}

/// What the one internal build path reports when it produces no value.
enum NoValue<E> {
    /// Another build holds the key, and this call did not wait or its budget ran out.
    Building,
    /// The wait was cancelled.
    Cancelled,
    /// This call's own build failed.
    Build(E),
}

/// A waiting caller's two bounds, carried together so neither can be supplied without the other.
struct Wait<'a, C: ?Sized> {
    cancel: &'a C,
    deadline: Instant,
}

impl<C: ?Sized> Clone for Wait<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C: ?Sized> Copy for Wait<'_, C> {}

/// How long a parked waiter sleeps before re-reading its [`Cancel`] source.
const WAIT_TICK: Duration = Duration::from_millis(50);

/// The wait budget a caller that never calls [`SingleFlightCache::set_wait_budget_ms`] gets.
///
/// Argued from the build it has to outlast: a racer can arrive at any point during a build
/// measured in seconds, so a budget at or below one build reinstates the refusal waiting exists to
/// remove.
pub const DEFAULT_WAIT_BUDGET_MS: u64 = 6_000;

/// What [`SingleFlightCache::peek`] found: a read that claims nothing.
///
/// The three states are distinguished because a caller answers them differently. `Building` means
/// a producer exists and the caller should fall back rather than start a second one; `Absent`
/// means nothing is coming and the caller may build.
pub enum Peek<V> {
    Ready(Arc<V>),
    Building,
    Absent,
}

/// Operator-facing cache gauges. Every field is read from an atomic without taking the slot lock,
/// so [`Self::slot_locks`] stays a measure of this type's own locking rather than of the caller's
/// polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Slots currently held, `Building` and `Ready` both counted.
    pub entries: usize,
    /// Charged bytes currently resident: `Σ max(weight, PER_ENTRY_FLOOR_BYTES)` over `Ready`
    /// slots. The floor makes this an over-estimate for small entries, and it is not process
    /// memory.
    pub bytes: u64,
    /// The configured byte bound, carried so an alarm on `bytes` needs no second input.
    pub bound_bytes: u64,
    pub hits: u64,
    /// Builds started. A losing arrival that gets [`Building`] is neither a hit nor a miss: it
    /// neither read a value nor built one.
    pub misses: u64,
    /// Callers turned away because a build was already in flight on their key: those through a
    /// non-waiting entry point, and those whose wait budget expired. A racer that waits and is
    /// served is counted by [`Self::waits_satisfied`] instead, so the two together are the race
    /// rate. Bounded by same-key concurrency: a working set that does not fit produces rebuilds,
    /// not refusals.
    pub building_refusals: u64,
    /// Waits that ended with the winner's value. A waiter that fell through to building its own
    /// value is in neither figure: it is a miss, because that is what it became.
    pub waits_satisfied: u64,
    /// Callers parked in a wait at this instant: a gauge, not a total. Process-wide and
    /// unattributed, so it says whether waiting occupies a caller's admission budget, never whose
    /// waiting does.
    pub waiters_now: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    /// Evictions of entries that were never reused (`uses == 0`).
    ///
    /// `evictions` alone cannot tell healthy eviction of cold entries from a working set that does
    /// not fit; both produce evictions at the same rate. An entry evicted before it was ever hit
    /// again is the signature of the second, so a sustained non-zero rate here is the thrash
    /// alarm.
    pub young_evictions: u64,
    /// Builds whose value alone exceeded the whole bound: served to their caller, never retained.
    /// Emptying the cache to admit one entry it still could not hold would be the worse answer.
    pub oversized_admissions: u64,
    /// Every acquisition of the slot mutex since construction, from anywhere in this module. It
    /// measures a property of the type rather than of one call path because
    /// [`SingleFlightCache::lock_slots`] is the one place that locks. The `try_lock` in
    /// `is_locked_now` is the one exception and is deliberately not counted.
    pub slot_locks: u64,
    /// Entries walked by [`SingleFlightCache::retain_keys`] passes, cumulatively. A prune is O(n)
    /// under the request-path lock, so this is what makes its cost observable.
    pub prune_scanned: u64,
}

/// The guarded state. One struct rather than several mutexes, so the map, the recency index and
/// the byte accounting cannot be updated apart.
struct Slots<K, V> {
    map: FxHashMap<Arc<K>, Slot<K, V>>,
    /// `Ready` entries only, ordered oldest-use first. `Building` slots are deliberately absent,
    /// which keeps `recency.len()` equal to the number of `Ready` slots and lets eviction take the
    /// front unconditionally.
    ///
    /// A `BTreeMap` keyed by a monotonic tick rather than an intrusive list over a slab: a touch
    /// is a remove and an insert, two O(log n) operations against O(1), on a request measured in
    /// milliseconds. A `last_used` field per slot with a sort at eviction time would instead be
    /// O(n log n) per pass under the request-path lock, over an n the floor permits to reach
    /// millions.
    recency: BTreeMap<u64, Arc<K>>,
    /// Monotonic and never reused. Uniqueness is what makes `Slot::Ready`'s `tick` a bijection
    /// with `recency`; reuse would let a touch remove another key's index entry, after which
    /// eviction takes a hot entry and a cold one becomes immortal.
    next_tick: u64,
    /// Monotonic build identity. See [`Slot::Building`]'s `seq`.
    next_seq: u64,
    /// `Σ charged` over `Ready` slots. Mirrors [`CacheStats::bytes`].
    bytes: u64,
}

impl<K: Eq + Hash, V> Slots<K, V> {
    /// The one place a slot is removed: eviction, pruning, a single-key evict, the publish's
    /// oversized arm and the failure guard all funnel here, and all four exits from `Building`
    /// that are removals are therefore woken here.
    ///
    /// Returns the removed value rather than dropping it, so the caller can drop it after
    /// releasing the lock. A `Building` slot has no value and returns `None` while still being
    /// removed.
    #[must_use = "the removed value must be carried out of the critical section and dropped \
                  after the lock is released, never dropped in place"]
    fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        match self.map.remove(key) {
            None => None,
            Some(Slot::Building { wake, .. }) => {
                // Issued with the map lock held, so it cannot land in the window between a waiter
                // deciding to sleep and sleeping. Each woken caller re-reads the map, finds the
                // slot gone, and becomes a builder itself.
                wake.notify_all();
                None
            }
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
    /// Debug-only because it is O(n). A shipped build has no assertion here, which is why nothing
    /// about eviction is allowed to depend on the bijection holding.
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

/// A map of independently single-flighted slots, bounded in bytes. See the module doc for the
/// concurrency shape and the five rules.
pub struct SingleFlightCache<K, V> {
    slots: Mutex<Slots<K, V>>,
    /// Read on every publish, and settable after construction because the value often comes from
    /// a configuration the construction site cannot see.
    bound_bytes: AtomicU64,
    /// The wait budget for [`SingleFlightCache::get_or_derive_waiting`], in milliseconds.
    wait_budget_ms: AtomicU64,
    entries: AtomicUsize,
    bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    building_refusals: AtomicU64,
    waits_satisfied: AtomicU64,
    waiters_now: AtomicU64,
    evictions: AtomicU64,
    evicted_bytes: AtomicU64,
    young_evictions: AtomicU64,
    oversized_admissions: AtomicU64,
    slot_locks: AtomicU64,
    prune_scanned: AtomicU64,
}

/// What one publish's eviction pass did, carried out of the critical section so the counters are
/// bumped without holding the lock.
#[derive(Default)]
struct EvictionTally {
    count: u64,
    bytes: u64,
    young: u64,
}

impl<K: Eq + Hash + Clone, V: CacheWeight> SingleFlightCache<K, V> {
    /// `bound_bytes` is the byte ceiling on resident entries. `u64::MAX` means no bound.
    pub fn new(bound_bytes: u64) -> Self {
        SingleFlightCache {
            slots: Mutex::new(Slots {
                map: FxHashMap::default(),
                recency: BTreeMap::new(),
                next_tick: 0,
                next_seq: 0,
                bytes: 0,
            }),
            bound_bytes: AtomicU64::new(bound_bytes),
            wait_budget_ms: AtomicU64::new(DEFAULT_WAIT_BUDGET_MS),
            entries: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            building_refusals: AtomicU64::new(0),
            waits_satisfied: AtomicU64::new(0),
            waiters_now: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            evicted_bytes: AtomicU64::new(0),
            young_evictions: AtomicU64::new(0),
            oversized_admissions: AtomicU64::new(0),
            slot_locks: AtomicU64::new(0),
            prune_scanned: AtomicU64::new(0),
        }
    }

    /// Set the byte bound. Setting it under load is sound and takes effect at the next publish.
    pub fn set_bound_bytes(&self, bound_bytes: u64) {
        self.bound_bytes.store(bound_bytes, Ordering::Relaxed);
    }

    /// Set the wait budget. Read once per waiting call, at entry, so a change takes effect for
    /// calls that arrive after it and never shortens a wait already in progress.
    pub fn set_wait_budget_ms(&self, wait_budget_ms: u64) {
        self.wait_budget_ms.store(wait_budget_ms, Ordering::Relaxed);
    }

    /// Slots currently held, `Building` and `Ready` both counted: a diagnostic, not a capacity
    /// bound.
    pub fn len(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// Whether the cache holds no slot at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The operator gauges. Lock-free, deliberately.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            bound_bytes: self.bound_bytes.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            building_refusals: self.building_refusals.load(Ordering::Relaxed),
            waits_satisfied: self.waits_satisfied.load(Ordering::Relaxed),
            waiters_now: self.waiters_now.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            evicted_bytes: self.evicted_bytes.load(Ordering::Relaxed),
            young_evictions: self.young_evictions.load(Ordering::Relaxed),
            oversized_admissions: self.oversized_admissions.load(Ordering::Relaxed),
            slot_locks: self.slot_locks.load(Ordering::Relaxed),
            prune_scanned: self.prune_scanned.load(Ordering::Relaxed),
        }
    }

    /// Look up `key` without waiting for a build already in flight, building a miss's value and
    /// offering it another key's entry to derive from.
    ///
    /// A hit clones the `Arc`, moves the entry to the young end of the recency order and returns
    /// without calling `make` at all. A miss makes this call the builder: publish `Building`, drop
    /// the lock, run `make` outside it, re-lock, evict to fit, publish `Ready`. A different
    /// concurrent miss on the same key gets `Err(Building)` immediately.
    ///
    /// `make` is handed `Some(source)` when `derive_from` names a key that is `Ready` at the
    /// moment this call claims its own slot, and `None` otherwise. The hint must be treated as
    /// one: a source that has been evicted, is still building or was never inserted yields `None`,
    /// and `make` must then produce exactly the value it would have produced from scratch. One
    /// closure taking an `Option` rather than two closures, because two closures are two places
    /// for the answers to diverge.
    ///
    /// The source is read inside the same acquisition that claims the target slot, so it cannot be
    /// evicted in between, and it is not touched for recency: deriving from an entry is not a use
    /// of it.
    ///
    /// Exactly two lock acquisitions on a miss, exactly one on a hit. The LRU touch and the
    /// eviction pass both happen inside those acquisitions and add none of their own.
    pub fn get_or_derive(
        &self,
        key: K,
        derive_from: Option<&K>,
        make: impl FnOnce(Option<&V>) -> V,
    ) -> Result<Arc<V>, Building> {
        let outcome = self.claim_and_build(
            key,
            derive_from,
            None::<Wait<'_, NeverCancelled>>,
            |source| Ok::<V, Infallible>(make(source)),
        );
        match outcome {
            Ok(value) => Ok(value),
            Err(NoValue::Building | NoValue::Cancelled) => Err(Building),
            Err(NoValue::Build(never)) => match never {},
        }
    }

    /// Look up `key`, waiting for a build already in flight rather than refusing. Everything else
    /// is [`Self::get_or_derive`]'s, because both are one function.
    ///
    /// Three ways out for a caller that finds a build in flight: the build publishes and this
    /// returns its value with `make` never running here; the build fails to publish, and this
    /// caller takes the slot and builds exactly as a fresh arrival would; or the budget expires or
    /// `cancel` reports the request gone.
    ///
    /// The budget is what bounds the wait. Rule 5 argues no build can leave a waiter unwoken; the
    /// budget is what makes that argument's failure a bounded stall rather than a hang.
    pub fn get_or_derive_waiting<C: Cancel + ?Sized>(
        &self,
        key: K,
        derive_from: Option<&K>,
        cancel: &C,
        make: impl FnOnce(Option<&V>) -> V,
    ) -> Result<Arc<V>, WaitEnded> {
        // Taken once, here, rather than per park: the budget bounds the whole call, so a waiter
        // that is woken, finds another builder and parks again cannot renew it. That is what makes
        // a succession of short-lived builders bounded rather than a livelock.
        let deadline =
            Instant::now() + Duration::from_millis(self.wait_budget_ms.load(Ordering::Relaxed));
        let outcome = self.claim_and_build(
            key,
            derive_from,
            Some(Wait { cancel, deadline }),
            |source| Ok::<V, Infallible>(make(source)),
        );
        match outcome {
            Ok(value) => Ok(value),
            Err(NoValue::Building) => Err(WaitEnded::Budget),
            Err(NoValue::Cancelled) => Err(WaitEnded::Cancelled),
            Err(NoValue::Build(never)) => match never {},
        }
    }

    /// Look up `key`, with a build that can fail, and without waiting for one in flight.
    ///
    /// If `build` returns `Err` or unwinds, this build's `Building` slot is removed before the
    /// call returns or the unwind propagates, so the key is left absent: never wedged at
    /// `Building`, and never a cached failure.
    pub fn get_or_try_build<E>(
        &self,
        key: K,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<Arc<V>, SingleFlightError<E>> {
        let outcome =
            self.claim_and_build(key, None, None::<Wait<'_, NeverCancelled>>, |_| build());
        match outcome {
            Ok(value) => Ok(value),
            // This call offers no wait, so there is no wait to cancel.
            Err(NoValue::Building | NoValue::Cancelled) => Err(SingleFlightError::Building),
            Err(NoValue::Build(e)) => Err(SingleFlightError::Build(e)),
        }
    }

    /// The one body behind every entry point. `wait` present means wait for an in-flight build;
    /// absent means refuse.
    fn claim_and_build<E, C: Cancel + ?Sized>(
        &self,
        key: K,
        derive_from: Option<&K>,
        wait: Option<Wait<'_, C>>,
        make: impl FnOnce(Option<&V>) -> Result<V, E>,
    ) -> Result<Arc<V>, NoValue<E>> {
        // Declared before any lock guard so that, whatever path is taken below, these drop after
        // the guard goes out of scope (rule 4). Every locked block below writes evicted values
        // here rather than dropping them in place.
        let mut dead: Vec<Arc<V>> = Vec::new();

        let (owned_key, seq, source) = {
            let mut slots = self.lock_slots();
            // Set by the first park, and read only to count a wait that ended in the winner's
            // value. A waiter that falls through to building its own is not a satisfied wait.
            let mut waited = false;
            loop {
                // Read before the `get_mut` borrow opens, so the whole touch happens inside the
                // one lookup. A tick read and not used costs nothing: only uniqueness and
                // monotonicity matter, never density.
                let new_tick = slots.next_tick;
                // `get_mut` by reference, never `entry(key.clone())`: with `Arc<K>` keys the entry
                // API would allocate on every call including a warm hit, which is the hot path.
                let hit = match slots.map.get_mut(&key) {
                    None => None,
                    Some(Slot::Building { wake, .. }) => {
                        let Some(wait) = wait else {
                            self.building_refusals.fetch_add(1, Ordering::Relaxed);
                            return Err(NoValue::Building);
                        };
                        let wake = Arc::clone(wake);
                        // `?` returns the budget or cancellation answer; `Ok` means woken, decide
                        // again from what the map says now, which is the loop, and which is also
                        // how a spurious wakeup is absorbed without renewing the budget.
                        slots = self.park(slots, &wake, wait)?;
                        waited = true;
                        continue;
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
                // Read under the same lock that is about to claim this key's slot, and
                // deliberately not a recency touch.
                let source = derive_from.and_then(|from| match slots.map.get(from) {
                    Some(Slot::Ready { value, .. }) => Some(Arc::clone(value)),
                    _ => None,
                });
                if let Some((value, slot_key, old_tick)) = hit {
                    // The rest of the touch, inside the same acquisition: the index moved to match
                    // the tick already stamped above. Both halves of the bijection updated
                    // together.
                    slots.next_tick += 1;
                    slots.recency.remove(&old_tick);
                    slots.recency.insert(new_tick, slot_key);
                    slots.check();
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    if waited {
                        self.waits_satisfied.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(value);
                }
                let owned_key = Arc::new(key.clone());
                let seq = slots.next_seq;
                slots.next_seq += 1;
                slots.map.insert(
                    Arc::clone(&owned_key),
                    Slot::Building {
                        seq,
                        wake: Arc::new(Condvar::new()),
                    },
                );
                self.entries.store(slots.map.len(), Ordering::Relaxed);
                slots.check();
                break (owned_key, seq, source);
            }
        };
        self.misses.fetch_add(1, Ordering::Relaxed);

        // Armed for the whole build; disarmed once the publish below has decided this slot's fate.
        // A build that returns `Err` or unwinds therefore always leaves its key absent rather than
        // stuck at `Building`, and, because the guard compares `seq`, never removes a later
        // builder's slot.
        let mut guard = RemoveUnlessPublished {
            cache: self,
            key: Arc::clone(&owned_key),
            seq,
            disarmed: false,
        };

        let value = Arc::new(match make(source.as_deref()) {
            Ok(built) => built,
            // The guard fires as this returns: the slot goes, its waiters are woken, and nothing
            // is cached.
            Err(e) => return Err(NoValue::Build(e)),
        });

        // Outside the lock, deliberately: a weight is O(containers) on a large bitmap, and
        // computing it inside the critical section would end the O(1) hold time this design rests
        // on.
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

        // `dead` drops here, with the lock released (rule 4).
        Ok(value)
    }

    /// Sleep on one `Building` slot's condvar until something displaces it, the budget runs out,
    /// or the caller's request is cancelled. Returns the re-acquired guard, and the caller
    /// re-reads the map: a wake is never treated as ready.
    ///
    /// Deliberately not `seq`-aware. A publish and a guard must compare `seq` because writing into
    /// another build's slot corrupts it. A waiter only reads, and the key wholly determines the
    /// value, so a `Ready` left by a later builder under the same key is exactly as correct an
    /// answer, and refusing it would rebuild a value already in the map.
    ///
    /// `wait_timeout` is capped at [`WAIT_TICK`] so the [`Cancel`] source, which has no wakeup
    /// path of its own, is re-read that often. The cost is a re-acquisition of a mutex held for
    /// O(1) against a budget measured in seconds.
    fn park<'a, E, C: Cancel + ?Sized>(
        &self,
        slots: MutexGuard<'a, Slots<K, V>>,
        wake: &Condvar,
        wait: Wait<'_, C>,
    ) -> Result<MutexGuard<'a, Slots<K, V>>, NoValue<E>> {
        // Both checked before sleeping, so a caller arriving with an already-expired budget or an
        // already-cancelled request never parks at all.
        if wait.cancel.is_cancelled() {
            return Err(NoValue::Cancelled);
        }
        let remaining = wait.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.building_refusals.fetch_add(1, Ordering::Relaxed);
            return Err(NoValue::Building);
        }

        self.waiters_now.fetch_add(1, Ordering::Relaxed);
        // Not counted through `lock_slots`, so counted here: `wait_timeout` re-acquires the mutex
        // without passing the choke point, and leaving it uncounted would make `slot_locks`
        // understate acquisitions the moment anyone waits.
        self.slot_locks.fetch_add(1, Ordering::Relaxed);
        let (slots, _timed_out) = wake
            .wait_timeout(slots, remaining.min(WAIT_TICK))
            .unwrap_or_else(PoisonError::into_inner);
        self.waiters_now.fetch_sub(1, Ordering::Relaxed);

        // `_timed_out` is deliberately unread: the tick expiring means re-check the cancellation
        // source, not give up, and the budget is re-tested at the top of the next park. Branching
        // on it here would end the wait at the first tick.
        Ok(slots)
    }

    /// The publish half of a miss. Runs with the lock held.
    ///
    /// Three outcomes, and the first two are the ones a natural implementation gets wrong:
    ///
    /// - this build's slot is gone, or carries another build's `seq`, so publish nothing.
    ///   Re-inserting would silently undo the removal.
    /// - the value alone exceeds the whole bound, so remove this build's `Building` slot and
    ///   publish nothing. Removing it is essential rather than tidiness: a `Building` slot with no
    ///   builder is a permanent refusal for that key.
    /// - otherwise, evict to fit and publish `Ready`.
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
        // Cloned out before anything below can displace this slot. The insertion at the end
        // overwrites `Building` in place rather than going through `Slots::remove`, so it is the
        // one exit from `Building` the removal choke point does not see, and the handle has to be
        // taken while the variant is still there to take it from.
        let wake = match slots.map.get(&**key) {
            Some(Slot::Building { seq: found, wake }) if *found == seq => Arc::clone(wake),
            _ => return EvictionTally::default(),
        };

        if charged > bound {
            // A `Building` slot: no value, so nothing to carry out of the critical section.
            let no_value = slots.remove(key);
            debug_assert!(no_value.is_none(), "a Building slot carries no value");
            self.oversized_admissions.fetch_add(1, Ordering::Relaxed);
            return EvictionTally::default();
        }

        // Evict before inserting, so the candidate cannot evict itself: `Building` is not in the
        // recency index, so it is not a candidate victim, and the loop targets a bound that
        // already leaves room for it.
        //
        // `pop_first`, not `values().next()`: taking the front entry out of the index here means
        // every iteration shrinks `recency` by exactly one, whatever the map says about that key,
        // so the loop is bounded by `recency.len()` unconditionally. Selecting without removing,
        // and relying on `Slots::remove` to unlink, makes termination depend on the bijection
        // holding, which is checked only under debug assertions.
        let mut tally = EvictionTally::default();
        while slots.bytes + charged > bound {
            let Some((_, victim)) = slots.recency.pop_first() else {
                // Nothing left to evict: every remaining slot is `Building`. The candidate is
                // admitted anyway rather than refused (rule 3), and the transient overshoot is
                // bounded by the in-flight builds, which hold their memory regardless of this map.
                break;
            };
            let (young, freed) = match slots.map.get(&*victim) {
                Some(Slot::Ready { uses, charged, .. }) => (*uses == 0, *charged),
                _ => (false, 0),
            };
            // Counted only when a value actually came back. A `None` here means the index named a
            // key the map does not hold as `Ready`, and counting it would put the operator's
            // gauges permanently ahead of the bytes actually freed: the reading that says the
            // bound is being enforced while it is not.
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
        // The only exit from `Building` that hands waiters a value, and after the insert, so a
        // waiter re-reading the map on wake sees `Ready` rather than the slot it went to sleep on.
        wake.notify_all();
        tally
    }

    /// Remove one key, if present. Returns whether anything was removed.
    ///
    /// A `Building` slot is removed too, and its waiters are woken; the build that owns it then
    /// publishes nothing, because the publish compares `seq`.
    pub fn evict(&self, key: &K) -> bool {
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
        drop(dead);
        removed
    }

    /// Remove every entry whose key `keep` rejects; returns how many were removed.
    ///
    /// The predicate sees keys only, and `Building` slots are removed too. Both halves are
    /// load-bearing: a predicate over values could not see a `Building` slot, so a prune would
    /// skip it and the publish that followed would find its own slot intact and publish an entry
    /// under a key the prune had just declared unreachable.
    ///
    /// This is an O(n) pass under the request-path lock, counted in
    /// [`CacheStats::prune_scanned`]. `keep` runs with the slot lock held, unlike a build, so it
    /// must be a pure function of the key: a predicate that could re-enter this cache
    /// self-deadlocks on a non-reentrant mutex.
    pub fn retain_keys(&self, keep: impl Fn(&K) -> bool) -> usize {
        let mut dead: Vec<Arc<V>> = Vec::new();
        let removed = {
            let mut slots = self.lock_slots();
            let doomed: Vec<Arc<K>> = slots
                .map
                .keys()
                .filter(|key| !keep(key))
                .map(Arc::clone)
                .collect();
            self.prune_scanned
                .fetch_add(slots.map.len() as u64, Ordering::Relaxed);
            for key in &doomed {
                if let Some(value) = slots.remove(key) {
                    dead.push(value);
                }
            }
            self.entries.store(slots.map.len(), Ordering::Relaxed);
            self.bytes.store(slots.bytes, Ordering::Relaxed);
            slots.check();
            doomed.len()
        };
        // `dead` drops here, with the lock released (rule 4).
        removed
    }

    /// Read `key` without claiming its slot, distinguishing the three states.
    ///
    /// [`Self::get_or_derive`] cannot serve this purpose: a miss there inserts `Building`, which
    /// commits the caller to producing a value and refuses every other arrival until it does.
    ///
    /// A hit touches recency, exactly as a hit through [`Self::get_or_derive`] does. A serve is a
    /// use, and counting it as one is what keeps an entry that is being served from being evicted
    /// underneath its caller as cold.
    pub fn peek(&self, key: &K) -> Peek<V> {
        let mut slots = self.lock_slots();
        let new_tick = slots.next_tick;
        let hit = match slots.map.get_mut(key) {
            None => return Peek::Absent,
            Some(Slot::Building { .. }) => return Peek::Building,
            Some(Slot::Ready {
                value,
                key: slot_key,
                tick,
                uses,
                ..
            }) => {
                *uses = uses.saturating_add(1);
                let old_tick = std::mem::replace(tick, new_tick);
                (Arc::clone(value), Arc::clone(slot_key), old_tick)
            }
        };
        let (value, slot_key, old_tick) = hit;
        slots.next_tick += 1;
        slots.recency.remove(&old_tick);
        slots.recency.insert(new_tick, slot_key);
        slots.check();
        self.hits.fetch_add(1, Ordering::Relaxed);
        Peek::Ready(value)
    }

    /// Every `Ready` entry as `(key, value)`, most recently used first.
    ///
    /// Taken from [`Slots::recency`] rather than by sorting the map: that index already holds
    /// exactly the `Ready` slots in oldest-use order, so this is a reversed walk of it rather than
    /// an O(n log n) pass under the request-path lock.
    ///
    /// Reading the list is not a use. Counting it as one would keep an entry a periodic reader
    /// visits young for ever, and the LRU would never reach it.
    pub fn ready_entries(&self) -> Vec<(K, Arc<V>)>
    where
        K: Clone,
    {
        let slots = self.lock_slots();
        slots
            .recency
            .values()
            .rev()
            .filter_map(|key| match slots.map.get(key) {
                Some(Slot::Ready { value, .. }) => Some(((**key).clone(), Arc::clone(value))),
                // Unreachable: `recency` indexes `Ready` slots only. Skipping is the fail-safe
                // direction, because a key a reader does not see rebuilds on the next request.
                _ => None,
            })
            .collect()
    }

    /// The only place this module takes the lock, and the reason [`CacheStats::slot_locks`] is a
    /// statement about the type rather than about one call path.
    ///
    /// A poisoned mutex is recovered from rather than propagated: nothing under this lock is a
    /// partially applied invariant a panic could leave torn, since [`Slots::remove`] is the only
    /// multi-structure mutation and it cannot panic between its three updates. Refusing every
    /// subsequent request because one unrelated thread unwound would fail closed on availability
    /// while buying no safety.
    fn lock_slots(&self) -> MutexGuard<'_, Slots<K, V>> {
        self.slot_locks.fetch_add(1, Ordering::Relaxed);
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the slot lock is held at this instant: the probe that makes rule 4 an assertion
    /// rather than a comment.
    ///
    /// Three caveats. It is a `try_lock`, so it cannot distinguish this thread holding the lock
    /// from another thread holding it, and only a single-threaded test may read it as "the caller
    /// holds it". A poisoned mutex also reports `true`, so a test using it must not run after an
    /// unrelated panic. And it is not counted in [`CacheStats::slot_locks`], because counting it
    /// would perturb the exact-count assertions it is used alongside.
    #[cfg(test)]
    fn is_locked_now(&self) -> bool {
        self.slots.try_lock().is_err()
    }
}

/// Removes this build's `Building` slot unless it published. Covers both failure exits, the `Err`
/// return and the unwind, with one mechanism.
///
/// Compares `seq`, not just state. A removal landing mid-build takes this slot; a later caller
/// then misses and inserts its own `Building` under the same key. A guard removing by key alone
/// would delete that later builder's slot, wedging it at a miss it never sees resolved.
struct RemoveUnlessPublished<'a, K: Eq + Hash + Clone, V: CacheWeight> {
    cache: &'a SingleFlightCache<K, V>,
    key: Arc<K>,
    seq: u64,
    disarmed: bool,
}

impl<K: Eq + Hash + Clone, V: CacheWeight> Drop for RemoveUnlessPublished<'_, K, V> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // This guard's own `drop` can run while a panic is already unwinding through it, so a
        // poisoned mutex must not be treated as a second panic here: that would abort the process
        // instead of completing the unwind. `lock_slots` recovers rather than propagating.
        let mut slots = self.cache.lock_slots();
        if matches!(slots.map.get(&*self.key), Some(Slot::Building { seq, .. }) if *seq == self.seq)
        {
            // A `Building` slot carries no value and no charged bytes, so this removal frees
            // nothing that could convoy. If this guard ever becomes able to remove a `Ready` slot,
            // the value must be carried out of the critical section like every other path; the
            // binding and the assertion are what make that change loud rather than silent.
            let no_value = slots.remove(&self.key);
            debug_assert!(no_value.is_none(), "a Building slot carries no value");
            self.cache.entries.store(slots.map.len(), Ordering::Relaxed);
            slots.check();
        }
    }
}

/// The floor must never become an under-charge as an entry's own bookkeeping grows. This checks
/// only the parts `size_of` can see; the opaque C-side allocation is precisely why the constant is
/// not derived from it. See [`PER_ENTRY_FLOOR_BYTES`].
const _: () = {
    assert!(
        std::mem::size_of::<u64>() * 4 + std::mem::size_of::<usize>() * 4
            <= PER_ENTRY_FLOOR_BYTES as usize
    );
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// The two-argument form most cases below are written in: no source key, so `make`'s `Option`
    /// is always `None`.
    trait GetOrBuild<K, V> {
        fn get_or_build(&self, key: K, build: impl FnOnce() -> V) -> Result<Arc<V>, Building>;
        fn get_or_build_waiting(
            &self,
            key: K,
            cancel: &TestCancel,
            build: impl FnOnce() -> V,
        ) -> Result<Arc<V>, WaitEnded>;
    }

    impl<K: Eq + std::hash::Hash + Clone, V: CacheWeight> GetOrBuild<K, V> for SingleFlightCache<K, V> {
        fn get_or_build(&self, key: K, build: impl FnOnce() -> V) -> Result<Arc<V>, Building> {
            self.get_or_derive(key, None, |source| {
                assert!(source.is_none(), "no source key was offered");
                build()
            })
        }

        fn get_or_build_waiting(
            &self,
            key: K,
            cancel: &TestCancel,
            build: impl FnOnce() -> V,
        ) -> Result<Arc<V>, WaitEnded> {
            self.get_or_derive_waiting(key, None, cancel, |source| {
                assert!(source.is_none(), "no source key was offered");
                build()
            })
        }
    }

    /// A cancellation source a test owns, standing in for whatever a caller's request carries.
    #[derive(Clone, Default)]
    struct TestCancel(Arc<AtomicBool>);

    impl TestCancel {
        fn new() -> Self {
            TestCancel::default()
        }

        fn cancel(&self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    impl Cancel for TestCancel {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    /// A generous bound on the builder handshakes below: long enough that no legitimate run ever
    /// approaches it, short enough that a regression reintroducing blocking fails with a clear
    /// panic instead of hanging the test binary until a timeout kills it.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

    /// A test value of a chosen weight. Every eviction assertion below needs entries whose sizes
    /// it controls.
    struct Weighed(u32, u64);

    impl CacheWeight for Weighed {
        fn cache_weight_bytes(&self) -> u64 {
            self.1
        }
    }

    /// Weight far above [`PER_ENTRY_FLOOR_BYTES`], so the floor is not what these tests measure.
    const BIG: u64 = 10_000;

    fn unbounded() -> SingleFlightCache<u32, Weighed> {
        SingleFlightCache::new(u64::MAX)
    }

    /// A hit must never call `build` — the closure panics if invoked, so any accidental rebuild on
    /// a warm key fails the test loudly rather than merely wasting work.
    #[test]
    fn a_ready_hit_never_calls_build_again() {
        let cache = unbounded();
        let first = cache.get_or_build(1, || Weighed(42, BIG)).unwrap();
        assert_eq!(first.0, 42);

        let second = cache
            .get_or_build(1, || panic!("must not rebuild a Ready key"))
            .unwrap();
        assert_eq!(second.0, 42);
        assert!(Arc::ptr_eq(&first, &second), "same Arc, not a fresh build");
    }

    /// The core claim, reproduced deterministically with no sleeps and no timing slack: a
    /// concurrent arrival on the same key while a build is in flight gets `Building` immediately
    /// rather than blocking, and once the build publishes, both the retried loser and a fresh
    /// arrival observe the built value without rebuilding.
    #[test]
    fn concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(HANDSHAKE_TIMEOUT)
                    .expect("builder never released — single-flight regression re-blocked it");
                Weighed(99, BIG)
            })
        });

        started_rx
            .recv_timeout(HANDSHAKE_TIMEOUT)
            .expect("builder never signalled start — single-flight regression re-blocked it");

        let loser = cache.get_or_build(1, || panic!("a losing arrival must not build"));
        assert!(matches!(loser, Err(Building)));
        assert_eq!(cache.stats().building_refusals, 1);

        release_tx.send(()).unwrap();
        let built = builder.join().unwrap().unwrap();
        assert_eq!(built.0, 99);

        let retried = cache
            .get_or_build(1, || panic!("must not rebuild once Ready"))
            .unwrap();
        assert_eq!(retried.0, 99);
    }

    /// Block until some caller is parked, or fail the test.
    ///
    /// Polls the gauge rather than sleeping a guessed interval. `waiters_now` is incremented
    /// before the caller sleeps and decremented on every wake, so a test that observes `1` knows a
    /// caller reached the wait. It dips to `0` between ticks, so this waits for the first
    /// observation rather than for a steady state.
    fn await_parked<V: CacheWeight>(cache: &SingleFlightCache<u32, V>) {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while cache.stats().waiters_now == 0 {
            assert!(
                Instant::now() < deadline,
                "no caller ever parked — the waiting path did not take the Building branch"
            );
            thread::yield_now();
        }
    }

    /// The property waiting exists to create: the racer is served the winner's value, and the
    /// expensive build ran exactly once. The build count is the half that a wait implemented as
    /// "sleep, then rebuild" would fail while still returning the right value.
    #[test]
    fn a_waiter_is_served_the_winners_value_and_the_build_runs_once() {
        let cache = Arc::new(unbounded());
        let builds = Arc::new(AtomicU64::new(0));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder_builds = Arc::clone(&builds);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                builder_builds.fetch_add(1, Ordering::Relaxed);
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(99, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let waiter_cache = Arc::clone(&cache);
        let waiter_builds = Arc::clone(&builds);
        let waiter = thread::spawn(move || {
            waiter_cache.get_or_build_waiting(1, &TestCancel::new(), move || {
                waiter_builds.fetch_add(1, Ordering::Relaxed);
                Weighed(0, BIG)
            })
        });
        await_parked(&cache);

        release_tx.send(()).unwrap();
        assert_eq!(builder.join().unwrap().unwrap().0, 99);
        assert_eq!(
            waiter.join().unwrap().unwrap().0,
            99,
            "the waiter must be served the winner's value, not its own"
        );
        assert_eq!(
            builds.load(Ordering::Relaxed),
            1,
            "the build must run once for both callers — that is single-flight"
        );

        let stats = cache.stats();
        assert_eq!(stats.waits_satisfied, 1);
        assert_eq!(stats.waiters_now, 0, "the gauge must return to zero");
        assert_eq!(
            stats.building_refusals, 0,
            "a served wait is not a refusal"
        );
    }

    /// The builder unwinds, the guard removes the slot, and the waiter is woken. It must not
    /// inherit the panic and must not hang: it sees what a fresh arrival would see, a miss, and
    /// builds. That the rebuild can panic again is correct, because no failure is cached and every
    /// caller finds out for itself.
    #[test]
    fn a_waiter_whose_build_panics_falls_back_to_building_it() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                builder_cache.get_or_build(1, move || -> Weighed {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                    panic!("boom");
                })
            }))
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let waiter_cache = Arc::clone(&cache);
        let waiter = thread::spawn(move || {
            waiter_cache.get_or_build_waiting(1, &TestCancel::new(), || Weighed(42, BIG))
        });
        await_parked(&cache);

        release_tx.send(()).unwrap();
        assert!(builder.join().unwrap().is_err(), "the panic must propagate");
        assert_eq!(
            waiter.join().unwrap().unwrap().0,
            42,
            "the waiter must build its own value after the winner's build died"
        );
        assert_eq!(cache.stats().waits_satisfied, 0, "no wait was satisfied");
    }

    /// The value exceeds the whole bound, so the publish removes the `Building` slot and publishes
    /// nothing. The waiter must see a miss and build: leaving the slot would be a permanent
    /// refusal for that key, which with waiters becomes a permanent stall.
    #[test]
    fn a_waiter_whose_build_is_oversized_falls_back_to_building_it() {
        // Bound below one entry: every build is oversized, served, never retained (rule 3).
        let cache = Arc::new(SingleFlightCache::<u32, Weighed>::new(BIG / 2));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(99, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let waiter_cache = Arc::clone(&cache);
        let waiter = thread::spawn(move || {
            waiter_cache.get_or_build_waiting(1, &TestCancel::new(), || Weighed(42, BIG))
        });
        await_parked(&cache);

        release_tx.send(()).unwrap();
        assert_eq!(builder.join().unwrap().unwrap().0, 99);
        assert_eq!(
            waiter.join().unwrap().unwrap().0,
            42,
            "nothing was published, so the waiter must build rather than stall"
        );
        assert_eq!(cache.len(), 0, "an oversized value is never retained");
        assert_eq!(cache.stats().oversized_admissions, 2);
    }

    /// A prune removes the `Building` slot mid-build. The waiter is woken, finds its key absent,
    /// and builds, and the original builder's publish is then a no-op because the `seq` check sees
    /// the waiter's slot rather than its own. Both halves matter: without the wake the waiter
    /// stalls to the budget, and without the `seq` check the prune is silently undone on top of
    /// the waiter's entry.
    #[test]
    fn a_waiter_whose_build_is_pruned_falls_back_to_building_it() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(99, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let waiter_cache = Arc::clone(&cache);
        let waiter = thread::spawn(move || {
            waiter_cache.get_or_build_waiting(1, &TestCancel::new(), || Weighed(42, BIG))
        });
        await_parked(&cache);

        assert_eq!(
            cache.retain_keys(|key| *key != 1),
            1,
            "the prune must remove the Building slot"
        );

        // The waiter now owns the slot and has published; only then is the builder released, so
        // its publish meets a slot carrying a different `seq`.
        let waited = waiter.join().unwrap().unwrap();
        assert_eq!(waited.0, 42);
        release_tx.send(()).unwrap();
        assert_eq!(builder.join().unwrap().unwrap().0, 99);
        assert_eq!(
            cache
                .get_or_build(1, || panic!("must not rebuild"))
                .unwrap()
                .0,
            42,
            "the pruned build must not publish over the waiter's entry"
        );
    }

    /// The budget is the bound, and it is what makes rule 5's argument's failure a stall rather
    /// than a hang. A build that never finishes yields the same refusal the immediate one did, and
    /// it is counted in the same place.
    #[test]
    fn a_waiter_that_is_never_satisfied_times_out_as_building() {
        let cache = Arc::new(unbounded());
        cache.set_wait_budget_ms(150);
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(99, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let started = Instant::now();
        let waited = cache.get_or_build_waiting(1, &TestCancel::new(), || {
            panic!("a timed-out waiter must not build")
        });
        assert!(matches!(waited, Err(WaitEnded::Budget)));
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the waiter returned before its budget — the tick must not end the wait"
        );
        assert_eq!(cache.stats().building_refusals, 1);

        release_tx.send(()).unwrap();
        builder.join().unwrap().unwrap();
    }

    /// A cancelled caller releases within a tick instead of holding its place to the budget. The
    /// budget here is two orders of magnitude longer than the assertion, so a waiter that ignored
    /// the cancellation would fail the elapsed check rather than merely being slow.
    #[test]
    fn a_cancelled_waiter_releases_before_the_budget() {
        let cache = Arc::new(unbounded());
        cache.set_wait_budget_ms(30_000);
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(99, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        let cancel = TestCancel::new();
        let waiter_cache = Arc::clone(&cache);
        let waiter_cancel = cancel.clone();
        let waiter = thread::spawn(move || {
            let started = Instant::now();
            let outcome = waiter_cache.get_or_build_waiting(1, &waiter_cancel, || {
                panic!("a cancelled waiter must not build")
            });
            (outcome, started.elapsed())
        });
        await_parked(&cache);

        cancel.cancel();
        let (outcome, elapsed) = waiter.join().unwrap();
        assert!(matches!(outcome, Err(WaitEnded::Cancelled)));
        assert!(
            elapsed < HANDSHAKE_TIMEOUT,
            "cancellation must release within a tick, not at the budget"
        );
        assert_eq!(
            cache.stats().building_refusals,
            0,
            "a caller's own disconnect is not backpressure and must not be counted as one"
        );

        release_tx.send(()).unwrap();
        builder.join().unwrap().unwrap();
    }

    /// A waiter woken by a removal may find that another caller has since claimed the key, and it
    /// then waits for that one instead of refusing: the budget, not the number of rounds, bounds
    /// the loop. The waiter is served the second builder's value, which is correct rather than
    /// tolerated, because the key wholly determines the value.
    #[test]
    fn a_waiter_takes_a_later_builders_value_for_the_same_key() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let first_cache = Arc::clone(&cache);
        let first = thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                first_cache.get_or_build(1, move || -> Weighed {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                    panic!("the first build dies without publishing");
                })
            }))
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // Two waiters on the same in-flight build. When it dies, one of them claims the slot and
        // the other necessarily meets a slot it never saw claimed — the interleaving under test.
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let waiter_cache = Arc::clone(&cache);
            waiters.push(thread::spawn(move || {
                waiter_cache.get_or_build_waiting(1, &TestCancel::new(), || Weighed(42, BIG))
            }));
        }
        await_parked(&cache);

        release_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_err());
        for waiter in waiters {
            assert_eq!(
                waiter.join().unwrap().unwrap().0,
                42,
                "every waiter must end with the key's value, built once or twice but never refused"
            );
        }
        assert_eq!(cache.len(), 1);
    }

    /// The map lock is held only for the O(1) transition, never for the build, proven by having
    /// key `1`'s build block indefinitely while key `2`'s build runs and completes on another
    /// thread. If the lock were held across the build, key `2` would hang.
    #[test]
    fn distinct_keys_never_contend_on_a_slow_build() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(HANDSHAKE_TIMEOUT)
                    .expect("builder never released — single-flight regression re-blocked it");
                Weighed(1, BIG)
            })
        });

        started_rx
            .recv_timeout(HANDSHAKE_TIMEOUT)
            .expect("builder never signalled start — single-flight regression re-blocked it");

        let other = cache.get_or_build(2, || Weighed(2, BIG)).unwrap();
        assert_eq!(other.0, 2);

        release_tx.send(()).unwrap();
        assert_eq!(slow.join().unwrap().unwrap().0, 1);
    }

    /// A panicking build must never leave a permanent `Building` wedge. The entry is absent
    /// afterwards, not `Building` and not a cached failure, so the very next call retries cleanly.
    #[test]
    fn a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache = unbounded();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_build(7, || -> Weighed { panic!("boom") })
        }));
        assert!(result.is_err(), "the panic must propagate to the caller");
        assert_eq!(cache.len(), 0, "a panicked build must not leave a wedge");

        let rebuilt = cache.get_or_build(7, || Weighed(7, BIG)).unwrap();
        assert_eq!(rebuilt.0, 7);
        assert_eq!(cache.len(), 1);
    }

    /// A build returning `Err` must never leave a permanent `Building` wedge, and the error must
    /// never be cached: the entry is absent afterwards, so the very next call retries cleanly and
    /// can succeed, unlike a cached failure.
    #[test]
    fn a_failed_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache = unbounded();

        let result = cache.get_or_try_build(7, || Err::<Weighed, _>("boom"));
        assert!(matches!(result, Err(SingleFlightError::Build("boom"))));
        assert_eq!(
            cache.len(),
            0,
            "a failed build must not leave a wedge, nor cache the Err"
        );

        let rebuilt = cache
            .get_or_try_build(7, || Ok::<_, &str>(Weighed(7, BIG)))
            .unwrap();
        assert_eq!(rebuilt.0, 7);
        assert_eq!(cache.len(), 1);
    }

    /// The counted choke point, hit path. A warm hit takes exactly one acquisition: the LRU touch
    /// happens inside it and adds none of its own. A recency index behind its own mutex, or a
    /// re-lock to update it, shows up here and nowhere else.
    #[test]
    fn a_hit_takes_exactly_one_lock() {
        let cache = unbounded();
        cache.get_or_build(1, || Weighed(1, BIG)).unwrap();

        let before = cache.stats().slot_locks;
        cache.get_or_build(1, || panic!("warm")).unwrap();
        assert_eq!(
            cache.stats().slot_locks - before,
            1,
            "a warm hit must take exactly one slot-lock acquisition"
        );
    }

    /// The counted choke point, miss path. Exactly two per miss: publish `Building`, then publish
    /// `Ready`. The eviction pass runs inside the second, and a third acquisition to evict after
    /// publishing fails here.
    #[test]
    fn a_miss_takes_exactly_two_locks() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache.get_or_build(1, || Weighed(1, BIG)).unwrap();

        let before = cache.stats().slot_locks;
        cache.get_or_build(2, || Weighed(2, BIG)).unwrap();
        // The third entry does not fit, so this miss also runs an eviction — inside its own two.
        cache.get_or_build(3, || Weighed(3, BIG)).unwrap();
        assert_eq!(
            cache.stats().slot_locks - before,
            4,
            "two misses must take exactly two acquisitions each, eviction included"
        );
        assert!(cache.stats().evictions >= 1, "the third entry must evict");
    }

    /// A value whose `Drop` observes the lock state records a violation if it is dropped inside
    /// the critical section. Deterministic and single-threaded: move the `dead` vector's drop
    /// inside the locked block and this fails every run.
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
                        // SAFETY: the pointer is set and cleared inside the test body below, and
                        // the cache outlives every value it holds.
                        if unsafe { &*cache }.is_locked_now() {
                            VIOLATION.with(|v| v.set(true));
                        }
                    }
                });
            }
        }

        let cache = SingleFlightCache::<u32, Tattle>::new(BIG * 2);
        PROBE.with(|probe| *probe.borrow_mut() = Some(&cache as *const _));

        cache.get_or_build(1, || Tattle(BIG)).unwrap();
        cache.get_or_build(2, || Tattle(BIG)).unwrap();
        // Forces an eviction. The evicted value is uniquely held by the cache — the `Arc`s
        // returned above were dropped at the end of each statement — so it really does drop here.
        cache.get_or_build(3, || Tattle(BIG)).unwrap();

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(cache.stats().evictions >= 1, "the test must have evicted");
        assert!(
            !VIOLATION.with(Cell::get),
            "an evicted value was dropped while the slot lock was held (rule 4): dropping a large \
             bitmap frees thousands of containers and convoys every waiting caller"
        );
    }

    /// Rule 4 on the pruning path, which the eviction case does not reach and which matters more:
    /// a prune frees a whole session's entries at once, so dropping them inside the critical
    /// section convoys every caller on the same mutex.
    #[test]
    fn pruned_arcs_are_dropped_outside_the_lock() {
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
                        // SAFETY: the pointer is set and cleared inside the test body below, and
                        // the cache outlives every value it holds.
                        if unsafe { &*cache }.is_locked_now() {
                            VIOLATION.with(|v| v.set(true));
                        }
                    }
                });
            }
        }

        let cache = SingleFlightCache::<u32, Tattle>::new(u64::MAX);
        PROBE.with(|probe| *probe.borrow_mut() = Some(&cache as *const _));

        // The returned `Arc`s are dropped at the end of each statement, so the cache is the unique
        // owner and the prune below really does drop these values.
        for key in 0..4u32 {
            cache.get_or_build(key, || Tattle(BIG)).unwrap();
        }

        assert_eq!(
            cache.retain_keys(|_| false),
            4,
            "the prune must remove all four"
        );

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert_eq!(cache.len(), 0);
        assert!(
            !VIOLATION.with(Cell::get),
            "a pruned value was dropped while the slot lock was held (rule 4)"
        );
    }

    /// Rule 4 on the single-key removal path. The values `evict` frees can be mapped files:
    /// dropping one inside the critical section unmaps hundreds of megabytes while every caller
    /// waits.
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
                        // SAFETY: the pointer is set and cleared inside the test body below, and
                        // the cache outlives every value it holds.
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
        cache.get_or_build(1, || Tattle(BIG)).unwrap();
        assert!(cache.evict(&1), "the entry was resident");

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(
            !VIOLATION.with(Cell::get),
            "an evicted value was dropped while the slot lock was held (rule 4)"
        );
    }

    /// Rule 1. A `Building` slot is never chosen as an eviction victim: it holds no value to free,
    /// and evicting it would let a second caller start a duplicate build.
    #[test]
    fn a_building_slot_is_never_evicted() {
        let cache = Arc::new(SingleFlightCache::<u32, Weighed>::new(BIG * 2));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(1, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // Fill and overfill the bound while key 1 is still Building.
        cache.get_or_build(2, || Weighed(2, BIG)).unwrap();
        cache.get_or_build(3, || Weighed(3, BIG)).unwrap();
        cache.get_or_build(4, || Weighed(4, BIG)).unwrap();

        // Key 1's slot must still be Building — a fresh arrival is refused, not made to build.
        let racer = cache.get_or_build(1, || panic!("key 1's Building slot was evicted"));
        assert!(matches!(racer, Err(Building)));

        release_tx.send(()).unwrap();
        assert_eq!(slow.join().unwrap().unwrap().0, 1);
    }

    /// A value larger than the whole bound is served to its caller (rule 3) and not retained, and
    /// its `Building` slot is removed, so the next arrival sees a plain miss rather than a
    /// permanent refusal.
    #[test]
    fn an_entry_larger_than_the_bound_is_served_but_not_retained() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG);

        let built = cache.get_or_build(1, || Weighed(5, BIG * 4)).unwrap();
        assert_eq!(built.0, 5, "rule 3: the builder receives what it built");
        assert_eq!(cache.stats().oversized_admissions, 1);
        assert_eq!(cache.stats().bytes, 0, "nothing was retained");
        assert_eq!(
            cache.len(),
            0,
            "the Building slot must be removed, not left behind: one with no builder is a \
             permanent refusal for that key"
        );

        // The wedge test proper: the very next arrival must be able to build, not be refused.
        let again = cache.get_or_build(1, || Weighed(6, BIG * 4));
        assert!(
            matches!(&again, Ok(v) if v.0 == 6),
            "a second arrival on an oversized key must rebuild, not get Building forever"
        );
    }

    /// The byte bound holds, and the victim is the least recently used, not the least recently
    /// inserted. Touching key 1 before inserting key 3 must make key 2 the victim.
    #[test]
    fn eviction_takes_the_least_recently_used() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache.get_or_build(1, || Weighed(1, BIG)).unwrap();
        cache.get_or_build(2, || Weighed(2, BIG)).unwrap();
        cache.get_or_build(1, || panic!("warm")).unwrap();

        cache.get_or_build(3, || Weighed(3, BIG)).unwrap();

        assert!(
            cache
                .get_or_build(1, || panic!("1 must still be resident"))
                .is_ok(),
            "the recently-used entry must survive"
        );
        let rebuilt = std::cell::Cell::new(false);
        cache
            .get_or_build(2, || {
                rebuilt.set(true);
                Weighed(2, BIG)
            })
            .unwrap();
        assert!(
            rebuilt.get(),
            "key 2 was the LRU victim and must have been evicted"
        );
    }

    /// `evict` removes one key and nothing else, in one acquisition.
    #[test]
    fn evict_removes_exactly_one_key() {
        let cache = unbounded();
        for key in 0..3u32 {
            cache.get_or_build(key, || Weighed(key, BIG)).unwrap();
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
            .get_or_build(1, || {
                rebuilt.set(true);
                Weighed(1, BIG)
            })
            .unwrap();
        assert!(rebuilt.get(), "the evicted key must rebuild");
    }

    /// The floor is what turns a byte bound into an entry bound. Entries far below it are charged
    /// the floor, so `bytes` grows at `n × floor` rather than at `n × weight`.
    #[test]
    fn tiny_entries_are_charged_the_floor() {
        let cache = SingleFlightCache::<u32, Weighed>::new(u64::MAX);
        for key in 0..10u32 {
            cache.get_or_build(key, || Weighed(key, 1)).unwrap();
        }
        assert_eq!(
            cache.stats().bytes,
            10 * PER_ENTRY_FLOOR_BYTES,
            "a byte bound that charges a 1-byte entry 1 byte bounds no number of entries"
        );
    }

    /// `young_evictions` counts only entries that were never reused, which is the thrash
    /// signature. An entry hit before being evicted is a healthy cold eviction and must not alarm.
    #[test]
    fn young_evictions_counts_only_never_reused_entries() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache.get_or_build(1, || Weighed(1, BIG)).unwrap();
        cache.get_or_build(2, || Weighed(2, BIG)).unwrap(); // key 2 is never reused
                                                            // The touch must come after key 2 is published, or key 1 is still the older entry in
                                                            // recency order and it, not key 2, is the first victim.
        cache.get_or_build(1, || panic!("warm")).unwrap(); // key 1 has been reused

        // Evicts key 2 — the LRU, never reused.
        cache.get_or_build(3, || Weighed(3, BIG)).unwrap();
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(
            cache.stats().young_evictions,
            1,
            "key 2 was evicted having never been reused"
        );

        // Evicts key 1 — reused once before it went cold.
        cache.get_or_build(4, || Weighed(4, BIG)).unwrap();
        assert_eq!(cache.stats().evictions, 2);
        assert_eq!(
            cache.stats().young_evictions,
            1,
            "key 1 was reused before eviction — a healthy cold eviction, not thrash"
        );
    }

    /// Pruning removes exactly the keys the predicate rejects, in one lock acquisition, and hands
    /// the values out to be dropped outside it.
    #[test]
    fn retain_keys_removes_only_rejected_keys_in_one_acquisition() {
        let cache = unbounded();
        for key in 0..6u32 {
            cache.get_or_build(key, || Weighed(key, BIG)).unwrap();
        }

        let before = cache.stats().slot_locks;
        let removed = cache.retain_keys(|key| key % 2 == 0);
        assert_eq!(removed, 3);
        assert_eq!(
            cache.stats().slot_locks - before,
            1,
            "a prune must take exactly one acquisition, not one per victim"
        );
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.stats().bytes, 3 * BIG);
        assert!(cache.get_or_build(0, || panic!("kept")).is_ok());
    }

    /// Rule 2. A prune landing mid-build must not be undone by the publish that follows it. The
    /// build completes and its caller receives the value (rule 3), but nothing is republished
    /// under the pruned key.
    #[test]
    fn a_prune_during_a_build_is_not_undone_by_the_publish() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(1, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // The prune must see and remove the Building slot — a predicate over values could not.
        assert_eq!(cache.retain_keys(|key| *key != 1), 1);
        release_tx.send(()).unwrap();

        let built = builder.join().unwrap().unwrap();
        assert_eq!(built.0, 1, "rule 3: the builder still receives its value");
        assert_eq!(
            cache.len(),
            0,
            "the publish must not resurrect a key the prune removed"
        );
    }

    /// Rule 2 on the single-key removal path: an `evict` landing mid-build must not be undone by
    /// the publish that follows it.
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

        assert!(cache.evict(&1), "the Building slot must be removed");
        release_tx.send(()).unwrap();

        let built = builder.join().unwrap().unwrap();
        assert_eq!(built.0, 1, "the builder still receives its value");
        assert_eq!(
            cache.len(),
            0,
            "the publish must not resurrect a key the evict removed"
        );
    }

    /// Rule 2, the sharper half on the publishing side. A build that succeeds after its slot was
    /// removed and a later builder claimed the key must publish nothing: writing its value into
    /// that builder's slot would overwrite a build in flight, and the later builder's own publish
    /// would then find a `Ready` slot and discard its value.
    #[test]
    fn a_build_whose_slot_was_reclaimed_does_not_publish_over_the_later_builder() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let first_cache = Arc::clone(&cache);
        let first = thread::spawn(move || {
            first_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(1, BIG)
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // The first builder's slot goes, and a second builder claims the key and stays in flight.
        assert!(cache.evict(&1), "the Building slot must be removed");
        let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
        let (second_release_tx, second_release_rx) = mpsc::channel::<()>();
        let second_cache = Arc::clone(&cache);
        let second = thread::spawn(move || {
            second_cache.get_or_build(1, move || {
                second_started_tx.send(()).unwrap();
                second_release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(2, BIG)
            })
        });
        second_started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // Now let the first build publish. It must leave the second builder's slot alone.
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap().0, 1, "rule 3 still holds");
        let racer = cache.get_or_build(1, || panic!("the second builder still owns this key"));
        assert!(
            matches!(racer, Err(Building)),
            "the first build published over the second builder's slot"
        );

        second_release_tx.send(()).unwrap();
        assert_eq!(second.join().unwrap().unwrap().0, 2);
        assert_eq!(
            cache.get_or_build(1, || panic!("must not rebuild")).unwrap().0,
            2,
            "the key must hold the second builder's value"
        );
    }

    /// Rule 2, the sharper half. A build that unwinds after its slot was pruned and a later
    /// builder claimed the key must not delete that later builder's slot.
    #[test]
    fn an_unwinding_build_does_not_delete_a_later_builders_slot() {
        let cache = Arc::new(unbounded());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let first_cache = Arc::clone(&cache);
        let first = thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                first_cache.get_or_build(1, move || -> Weighed {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                    panic!("first build fails")
                })
            }))
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // A prune removes the first builder's slot; a second builder then claims the same key.
        assert_eq!(cache.retain_keys(|key| *key != 1), 1);
        let (second_started_tx, second_started_rx) = mpsc::channel::<()>();
        let (second_release_tx, second_release_rx) = mpsc::channel::<()>();
        let second_cache = Arc::clone(&cache);
        let second = thread::spawn(move || {
            second_cache.get_or_build(1, move || {
                second_started_tx.send(()).unwrap();
                second_release_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
                Weighed(2, BIG)
            })
        });
        second_started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

        // Now let the first build unwind. Its guard must leave the second builder's slot alone.
        release_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_err());

        let racer = cache.get_or_build(1, || panic!("the second builder still owns this key"));
        assert!(
            matches!(racer, Err(Building)),
            "the unwinding first build deleted the second builder's slot"
        );

        second_release_tx.send(()).unwrap();
        assert_eq!(second.join().unwrap().unwrap().0, 2);
    }

    /// Rule 2, the same half reached without a panic: a build that returns `Err` after its slot
    /// was evicted, and after a later builder claimed the key, must not delete that builder's
    /// slot. A fallible build reaches this interleaving through an ordinary runtime failure.
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
                Err("the build failed")
            })
        });
        started_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();

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
            Err(SingleFlightError::Build("the build failed"))
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

    /// Forward progress under a bound below the working set. Five keys round-robin through a cache
    /// that holds two: every call still returns its own value, and none is ever refused, because
    /// refusals come from same-key concurrency, which a round-robin never creates.
    ///
    /// It is also the liveness test: an eviction loop that re-selected a victim it never removed
    /// would hang here under the request-path mutex in a release build, where the bijection check
    /// compiles to nothing.
    #[test]
    fn an_undersized_bound_does_not_livelock() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        for round in 0..5 {
            for key in 0..5u32 {
                let got = cache
                    .get_or_build(key, || Weighed(key, BIG))
                    .unwrap_or_else(|_| panic!("round {round} key {key} was refused"));
                assert_eq!(got.0, key, "every caller receives its own value");
            }
        }
        assert_eq!(
            cache.stats().building_refusals,
            0,
            "a bound below the working set must cost rebuilds, never refusals (rule 3)"
        );
        assert!(cache.stats().bytes <= BIG * 2, "the bound held throughout");
    }

    /// `ready_entries` is most recently used first, and reading it is not a use. The order decides
    /// which callers wait longest when a serial reader walks the list, and it must be the ones
    /// that asked least recently.
    ///
    /// The mutations this kills: iterating the map instead of the index (leg 1 fails on order, or
    /// flakes, which is itself the point); dropping the `.rev()` (leg 1 reverses); touching
    /// recency inside `ready_entries` (leg 2 sees the order change under a read).
    #[test]
    fn ready_entries_are_most_recently_used_first_and_reading_them_is_not_a_use() {
        let cache = unbounded();
        for key in 0..4u32 {
            cache.get_or_build(key, || Weighed(key, BIG)).unwrap();
        }
        // Re-touch 0 and 1, so use order is 2, 3, 0, 1 oldest-first.
        cache.get_or_build(0, || panic!("warm")).unwrap();
        cache.get_or_build(1, || panic!("warm")).unwrap();

        let order: Vec<u32> = cache.ready_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            order,
            vec![1, 0, 3, 2],
            "the most recently used entry must come first"
        );

        // Leg 2: a second read sees the same order. A recency touch here would make the entry a
        // periodic reader visits immortal, since the LRU would never reach it.
        let again: Vec<u32> = cache.ready_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(again, order, "reading the list must not reorder it");
    }
}
