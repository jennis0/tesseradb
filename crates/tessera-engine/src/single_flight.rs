//! D-G's slot-state single-flight cache.
//!
//! The row-projection cache's original shape (`Mutex<FxHashMap<Key, Arc<RowProjection>>>`) ran
//! `RowProjection::new` — the entity-space-to-row-space crossing, seconds at 10⁹ rows
//! (shared-context constraint 8) — **inside** the map lock on a miss, so every distinct session's
//! first viewport serialised behind one global mutex (F4, measured: `tessera-bench/src/arms/
//! load.rs:34-76` — Arm A at c=1000 throughput halves and server CPU *drops* while p99 reaches
//! 1.04 s, the signature of threads blocked on a lock, not doing work).
//!
//! This module is the replacement: a slot per key is either [`Slot::Building`] or
//! [`Slot::Ready`], and the map's mutex is held only for the O(1) transition between those states
//! — never for the build itself, which always runs with the lock released. A concurrent arrival
//! on the *same* key while a build is in flight does not wait for it: D-G's non-blocking-waiters
//! rule is that a parked waiter would hold the server's global admission budget (a later task)
//! while consuming zero CPU, so a queue of waiters would starve runnable work exactly when the
//! server is under the load that makes it worst. [`SingleFlightCache::get_or_build`] therefore
//! returns [`Building`] immediately to a losing arrival, and the caller decides what that means
//! (`Engine::viewport` turns it into `EngineError::ProjectionBuilding`).
//!
//! Generic over `K`/`V` and free of any `RowProjection`/`Engine` knowledge, so the state machine
//! itself is unit-testable here with a trivial `V` and a controllable `build` closure, without
//! opening a bundle — see this module's tests for the deterministic (channel-synchronised, not
//! timing-dependent) reproductions of single-flight, non-blocking-waiter and panic-safety
//! behaviour.

use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

/// One key's state. There is deliberately no third, "failed" state: D-G requires a failed
/// (panicking) build to remove the entry outright rather than cache anything for it, so the next
/// arrival retries — caching a failure would be a permanent fail-closed wedge (I13).
enum Slot<V> {
    Building,
    Ready(Arc<V>),
}

/// A losing arrival's outcome: some other caller is already building this key, and this call did
/// not wait for it (D-G). Carries nothing — the caller only needs to know to retry, never a
/// handle to the in-flight build.
#[derive(Debug)]
pub(crate) struct Building;

/// A map of independently single-flighted slots. See the module doc for the concurrency shape.
pub(crate) struct SingleFlightCache<K, V> {
    slots: Mutex<FxHashMap<K, Slot<V>>>,
}

impl<K: Eq + Hash + Clone, V> SingleFlightCache<K, V> {
    pub(crate) fn new() -> Self {
        SingleFlightCache {
            slots: Mutex::new(FxHashMap::default()),
        }
    }

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic, not a capacity
    /// bound (unbounded-map eviction is out of scope for this cache; a memory concern, not a
    /// concurrency one — see the D-G task's commit note).
    pub(crate) fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Look up `key`. A hit clones the `Arc` and returns without calling `build` at all. A miss
    /// makes this call the builder: publish `Building`, drop the lock, run `build()` outside it,
    /// re-lock, publish `Ready`. A *different* concurrent miss on the same key observed while this
    /// is in flight gets `Err(Building)` immediately (see this module's doc for why that is
    /// correct rather than a shortcut).
    ///
    /// **Panic safety (I13).** If `build` unwinds, a drop guard removes the `Building` entry
    /// before the unwind propagates to this call's caller, so the key is left absent — not
    /// wedged — and the next arrival sees a plain miss and retries.
    pub(crate) fn get_or_build(
        &self,
        key: K,
        build: impl FnOnce() -> V,
    ) -> Result<Arc<V>, Building> {
        {
            let mut slots = self.slots.lock().unwrap();
            match slots.entry(key.clone()) {
                Entry::Occupied(occupied) => {
                    return match occupied.get() {
                        Slot::Ready(v) => Ok(Arc::clone(v)),
                        Slot::Building => Err(Building),
                    };
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(Slot::Building);
                }
            }
        }

        // Armed for the whole build; disarmed only after `Ready` is published below. An
        // unwinding `build` therefore always leaves `key` absent rather than stuck at `Building`
        // or wrongly `Ready` — the removal happens on the unwind path through this guard's `Drop`.
        struct RemoveOnUnwind<'a, K: Eq + Hash, V> {
            slots: &'a Mutex<FxHashMap<K, Slot<V>>>,
            key: K,
            disarmed: bool,
        }
        impl<K: Eq + Hash, V> Drop for RemoveOnUnwind<'_, K, V> {
            fn drop(&mut self) {
                if !self.disarmed {
                    self.slots.lock().unwrap().remove(&self.key);
                }
            }
        }
        let mut guard = RemoveOnUnwind {
            slots: &self.slots,
            key: key.clone(),
            disarmed: false,
        };

        let value = Arc::new(build());

        self.slots
            .lock()
            .unwrap()
            .insert(key, Slot::Ready(Arc::clone(&value)));
        guard.disarmed = true;

        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// A hit must never call `build` — the closure panics if invoked, so any accidental rebuild
    /// on a warm key fails the test loudly rather than merely wasting work.
    #[test]
    fn a_ready_hit_never_calls_build_again() {
        let cache: SingleFlightCache<u32, u32> = SingleFlightCache::new();
        let first = cache.get_or_build(1, || 42).unwrap();
        assert_eq!(*first, 42);

        let second = cache
            .get_or_build(1, || panic!("must not rebuild a Ready key"))
            .unwrap();
        assert_eq!(*second, 42);
        assert!(Arc::ptr_eq(&first, &second), "same Arc, not a fresh build");
    }

    /// D-G's core claim, reproduced deterministically (no sleeps, no timing slack): a concurrent
    /// arrival on the same key while a build is in flight gets `Building` immediately rather than
    /// blocking, and once the build publishes `Ready`, both the retried loser and a fresh arrival
    /// observe the built value without rebuilding.
    #[test]
    fn concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild() {
        let cache = Arc::new(SingleFlightCache::<u32, u32>::new());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let builder_cache = Arc::clone(&cache);
        let builder = thread::spawn(move || {
            builder_cache.get_or_build(1, move || {
                // `Building` is published (under the map lock) strictly before this closure
                // runs, so by the time the main thread receives on `started_rx` the state this
                // test wants to race against already exists.
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                99
            })
        });

        started_rx.recv().unwrap();

        // Non-blocking: this call returns immediately (it does not wait on `release_tx`) with
        // `Building`, and its own closure must never run — there is already a builder for `1`.
        let loser = cache.get_or_build(1, || panic!("a losing arrival must not build"));
        assert!(matches!(loser, Err(Building)));

        release_tx.send(()).unwrap();
        let built = builder.join().unwrap().unwrap();
        assert_eq!(*built, 99);

        // The retried loser, and any fresh arrival, now hit `Ready` without rebuilding.
        let retried = cache
            .get_or_build(1, || panic!("must not rebuild once Ready"))
            .unwrap();
        assert_eq!(*retried, 99);
    }

    /// The map lock is held only for the O(1) transition, never for the build — proven here by
    /// having key `1`'s build block indefinitely (until released at the end of the test) while
    /// key `2`'s build runs and completes on another thread. If the lock were held across the
    /// build (the anti-fix the F4 memo names — merely narrowing the critical section without
    /// moving the build outside it), key `2` would hang waiting for key `1`'s lock to be released.
    #[test]
    fn distinct_keys_never_contend_on_a_slow_build() {
        let cache = Arc::new(SingleFlightCache::<u32, u32>::new());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                1
            })
        });

        started_rx.recv().unwrap();

        // A distinct key's build must complete without waiting on key 1's release.
        let other = cache.get_or_build(2, || 2).unwrap();
        assert_eq!(*other, 2);

        release_tx.send(()).unwrap();
        assert_eq!(*slow.join().unwrap().unwrap(), 1);
    }

    /// I13: a panicking build must never leave a permanent `Building` wedge. The entry is absent
    /// afterwards (not `Building`, not a cached failure), so the very next call retries cleanly.
    #[test]
    fn a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache: SingleFlightCache<u32, u32> = SingleFlightCache::new();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_build(7, || panic!("boom"))
        }));
        assert!(result.is_err(), "the panic must propagate to the caller");
        assert_eq!(
            cache.len(),
            0,
            "a panicked build must not leave a Building wedge (I13)"
        );

        let rebuilt = cache.get_or_build(7, || 7).unwrap();
        assert_eq!(*rebuilt, 7);
        assert_eq!(cache.len(), 1);
    }
}
