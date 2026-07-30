//! D-G's slot-state single-flight cache, fallible form (lifecycle §3.3).
//!
//! Same shape as `tessera-engine::single_flight::SingleFlightCache` (commit d9baada, Task 1 of
//! this concurrency workstream): a slot per key is either [`Slot::Building`] or [`Slot::Ready`],
//! the map's mutex is held only for the O(1) transition between those states — never across the
//! build itself — and a concurrent arrival on the same key does not wait for an in-flight build:
//! it gets [`SingleFlightError::Building`] immediately (D-G's non-blocking-waiters rule: a parked
//! waiter would hold the server's admission budget while burning zero CPU).
//!
//! **Duplicated here rather than reused**, deliberately: `tessera-authz` sits *below*
//! `tessera-engine` in the crate graph (engine depends on authz, per `crates/tessera-engine/
//! Cargo.toml`), and `scripts/check-layers.sh` enforces that direction, so this crate cannot take
//! a dependency on `tessera-engine` to reuse its module. The two use sites also need different
//! `get_or_build` signatures: the row-projection build engine's cache wraps is infallible, while
//! the fragment build this cache wraps is `io::Result` — a shared module would need exactly the
//! generalisation ([`get_or_try_build`](SingleFlightCache::get_or_try_build)) this module carries
//! anyway. See `tessera-engine/src/single_flight.rs`'s module doc for the fuller design rationale
//! (the F4 measurement — lock-held-across-build serialising every session's first request behind
//! one mutex — this pattern answers) and its tests for the same interleavings reproduced there
//! with an infallible builder.

use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

/// One key's state. There is deliberately no third, "failed" state: a failed (`Err`-returning or
/// panicking) build must remove the entry outright rather than cache anything for it, so the next
/// arrival retries — caching a failure would be a permanent fail-closed wedge for that credential
/// (I13).
enum Slot<V> {
    Building,
    Ready(Arc<V>),
}

/// A losing (or unlucky) arrival's outcome.
#[derive(Debug)]
pub(crate) enum SingleFlightError<E> {
    /// Another caller is already building this key right now; this call did not wait for it
    /// (D-G). The caller decides what that means — `FragmentCache::get_or_build` surfaces it as
    /// `FragmentCacheError::Building`, which `Engine::authorise` turns into
    /// `EngineError::FragmentBuilding`.
    Building,
    /// The build that *this* call ran failed. The entry has already been removed (never cached —
    /// I13 fail-closed) by the time this is returned, so the next arrival sees a plain miss.
    Build(E),
}

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

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic (fail-closed
    /// tests confirm a failed build leaves this at the count it started at, never wedged), not a
    /// capacity bound. `FragmentCache::slot_count` re-exports this publicly.
    pub(crate) fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Look up `key`. A hit clones the `Arc` and returns without calling `build` at all — this is
    /// the in-memory cache half of D-G. A miss makes this call the builder: publish `Building`,
    /// drop the lock, run `build()` outside it, re-lock, publish `Ready` on `Ok`. A *different*
    /// concurrent miss on the same key observed while this is in flight gets
    /// `Err(SingleFlightError::Building)` immediately — see the module doc for why that is
    /// correct rather than a shortcut. A re-entrant lookup during a build (the same thread calling
    /// back in from inside its own `build`) sees the same `Building` state and errors rather than
    /// deadlocking, for the same reason: the map lock is never held across `build`.
    ///
    /// **Fail-closed (I13).** If `build` returns `Err` or unwinds, a drop guard removes the
    /// `Building` entry before this call returns (or the unwind propagates), so the key is left
    /// absent — never wedged at `Building`, never a cached `Err` — and the next arrival sees a
    /// plain miss and retries.
    pub(crate) fn get_or_try_build<E>(
        &self,
        key: K,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<Arc<V>, SingleFlightError<E>> {
        {
            let mut slots = self.slots.lock().unwrap();
            match slots.entry(key.clone()) {
                Entry::Occupied(occupied) => {
                    return match occupied.get() {
                        Slot::Ready(v) => Ok(Arc::clone(v)),
                        Slot::Building => Err(SingleFlightError::Building),
                    };
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(Slot::Building);
                }
            }
        }

        // Armed for the whole build; disarmed only after `Ready` is published below. Both an
        // `Err` return and an unwinding `build` leave `key` absent rather than stuck at
        // `Building` or wrongly `Ready` — the removal happens through this guard's `Drop` on
        // *any* early exit, not just the panic path (unlike the infallible engine cache, this one
        // has two failure exits to cover with one mechanism).
        struct RemoveUnlessReady<'a, K: Eq + Hash, V> {
            slots: &'a Mutex<FxHashMap<K, Slot<V>>>,
            key: K,
            ready: bool,
        }
        impl<K: Eq + Hash, V> Drop for RemoveUnlessReady<'_, K, V> {
            fn drop(&mut self) {
                if !self.ready {
                    self.slots.lock().unwrap().remove(&self.key);
                }
            }
        }
        let mut guard = RemoveUnlessReady {
            slots: &self.slots,
            key: key.clone(),
            ready: false,
        };

        let built = match build() {
            Ok(v) => v,
            Err(e) => return Err(SingleFlightError::Build(e)),
        };

        let value = Arc::new(built);
        self.slots
            .lock()
            .unwrap()
            .insert(key, Slot::Ready(Arc::clone(&value)));
        guard.ready = true;

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
        let first = cache.get_or_try_build(1, || Ok::<_, ()>(42)).unwrap();
        assert_eq!(*first, 42);

        let second = cache
            .get_or_try_build(1, || -> Result<u32, ()> { panic!("must not rebuild a Ready key") })
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
            builder_cache.get_or_try_build(1, move || {
                // `Building` is published (under the map lock) strictly before this closure
                // runs, so by the time the main thread receives on `started_rx` the state this
                // test wants to race against already exists.
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok::<_, ()>(99)
            })
        });

        started_rx.recv().unwrap();

        // Non-blocking: this call returns immediately (it does not wait on `release_tx`) with
        // `Building`, and its own closure must never run — there is already a builder for `1`.
        let loser = cache.get_or_try_build(1, || -> Result<u32, ()> {
            panic!("a losing arrival must not build")
        });
        assert!(matches!(loser, Err(SingleFlightError::Building)));

        release_tx.send(()).unwrap();
        let built = builder.join().unwrap().unwrap();
        assert_eq!(*built, 99);

        // The retried loser, and any fresh arrival, now hit `Ready` without rebuilding.
        let retried = cache
            .get_or_try_build(1, || -> Result<u32, ()> { panic!("must not rebuild once Ready") })
            .unwrap();
        assert_eq!(*retried, 99);
    }

    /// The map lock is held only for the O(1) transition, never for the build — proven here by
    /// having key `1`'s build block indefinitely (until released at the end of the test) while
    /// key `2`'s build runs and completes on another thread. If the lock were held across the
    /// build, key `2` would hang waiting for key `1`'s lock to be released.
    #[test]
    fn distinct_keys_never_contend_on_a_slow_build() {
        let cache = Arc::new(SingleFlightCache::<u32, u32>::new());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let slow_cache = Arc::clone(&cache);
        let slow = thread::spawn(move || {
            slow_cache.get_or_try_build(1, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok::<_, ()>(1)
            })
        });

        started_rx.recv().unwrap();

        // A distinct key's build must complete without waiting on key 1's release.
        let other = cache.get_or_try_build(2, || Ok::<_, ()>(2)).unwrap();
        assert_eq!(*other, 2);

        release_tx.send(()).unwrap();
        assert_eq!(*slow.join().unwrap().unwrap(), 1);
    }

    /// I13: a build returning `Err` must never leave a permanent `Building` wedge, and the error
    /// must never be cached — the entry is absent afterwards, so the very next call retries
    /// cleanly (and can succeed, unlike a cached failure, which would be a permanent fail-closed
    /// wedge for that key).
    #[test]
    fn a_failed_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache: SingleFlightCache<u32, u32> = SingleFlightCache::new();

        let result = cache.get_or_try_build(7, || Err::<u32, _>("boom"));
        assert!(matches!(result, Err(SingleFlightError::Build("boom"))));
        assert_eq!(
            cache.len(),
            0,
            "a failed build must not leave a Building wedge, nor cache the Err (I13)"
        );

        let rebuilt = cache.get_or_try_build(7, || Ok::<_, &str>(7)).unwrap();
        assert_eq!(*rebuilt, 7);
        assert_eq!(cache.len(), 1);
    }

    /// I13, panic form: same guarantee as the `Err` case above, but via unwinding rather than a
    /// returned `Err` — both early-exit paths share the one drop-guard mechanism.
    #[test]
    fn a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache: SingleFlightCache<u32, u32> = SingleFlightCache::new();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_try_build(7, || -> Result<u32, ()> { panic!("boom") })
        }));
        assert!(result.is_err(), "the panic must propagate to the caller");
        assert_eq!(
            cache.len(),
            0,
            "a panicked build must not leave a Building wedge (I13)"
        );

        let rebuilt = cache.get_or_try_build(7, || Ok::<_, ()>(7)).unwrap();
        assert_eq!(*rebuilt, 7);
        assert_eq!(cache.len(), 1);
    }
}
