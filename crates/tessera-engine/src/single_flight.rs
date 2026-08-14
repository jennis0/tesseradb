//! A slot-state single-flight cache, with a byte bound, LRU and pruning.
//!
//! The obvious shape (`Mutex<FxHashMap<Key, Arc<RowProjection>>>`) runs
//! `RowProjection::new` — the entity-space-to-row-space crossing, seconds at 10⁹ rows —
//! **inside** the map lock on a miss, so every distinct session's
//! first viewport serialises behind one global mutex. Measured (`tessera-bench`'s load arm, Arm A
//! at c=1000): throughput halves and server CPU *drops* while p99 reaches
//! 1.04 s — the signature of threads blocked on a lock, not doing work.
//!
//! This module is the replacement: a slot per key is either [`Slot::Building`] or
//! [`Slot::Ready`], and the map's mutex is held only for the O(1) transition between those states
//! — never for the build itself, which always runs with the lock released.
//!
//! **A concurrent arrival on the same key waits for the in-flight build and is served its result**
//! (decision 0058), through [`SingleFlightCache::get_or_derive_waiting`]. The earlier rule was that
//! nobody waited, on the ground that a parked waiter holds the server's admission budget while
//! burning no CPU. That argument survives as a *cost* and is why the wait is bounded and
//! cancellable, but it was the wrong answer to the case that motivated it: the losing caller is
//! refused work that is already succeeding on another thread, and at 10⁹ the build outlasts the
//! client's whole retry budget, so the user gets a blank map from a server doing nothing heavy.
//! Decision 0059 records why the slot occupancy this buys is bounded by the wait budget rather than
//! by a per-principal cap.
//!
//! [`SingleFlightCache::get_or_derive`] keeps the non-waiting behaviour and returns [`Building`]
//! immediately, because one caller must not wait: the background refresh runs **on a rayon worker**
//! and the build it would park behind calls `pool.install`, so parking workers on work that needs
//! workers is a starvation deadlock, not merely a wasted refresh slot. The request path resolves on
//! its own calling thread (`Engine::viewport`'s D-D guardrail) and is free to block.
//!
//! # The bound, and the four rules that make eviction safe
//!
//! Eviction is **not invalidation**. Lifecycle §7 requires cache entries to be immutable and
//! "invalidation is key rotation, never mutation" — nothing here ever modifies a cached value.
//! What this module adds is capacity management (evicting a still-correct entry to stay under a
//! byte bound) and pruning ([`SingleFlightCache::retain_keys`] — removing entries whose key no
//! request can produce any more). Both only ever *remove*, and a removal can never widen a mask:
//! the miss path rebuilds from the same inputs the key names.
//!
//! Four rules carry it, each guarding a failure the natural implementation walks into:
//!
//! 1. **A `Building` slot is never evicted, weighs nothing, and is not in the recency index.**
//!    Evicting one frees nothing (the value does not exist yet; the memory is on the builder's
//!    stack) and loses the single-flight property — a second caller would start a duplicate
//!    multi-second build. Keeping `Building` *out of the index entirely* is what makes rule 1 and
//!    the index's bijection compose: an eviction pass that had to pop-then-skip a `Building` key
//!    would leave a slot holding a tick with no index entry, permanently unevictable while still
//!    charged, until `bytes` sits at the bound made entirely of entries none of which can be
//!    chosen as a victim.
//!
//!    **The consequence of breaking it is a hang, and an earlier draft of this doc understated
//!    it.** That draft said only "the bound stops holding and the counters say it is holding". The
//!    truth is worse: the eviction loop below used to select its victim with
//!    `recency.values().next()` and rely on [`Slots::remove`] to unlink it, so a victim whose slot
//!    was not `Ready` was re-selected for ever — **spinning with the request-path mutex held**.
//!    [`Slots::check`] is `#[cfg(debug_assertions)]`, so a `--release` build has no assertion to
//!    trip: `an_undersized_bound_does_not_livelock`, the test named for exactly this, *hangs*
//!    rather than fails. The loop now takes its victim with `BTreeMap::pop_first`, which removes
//!    the index entry unconditionally, so every iteration shrinks `recency` by one and termination
//!    is a property of the loop rather than of rule 1. Rule 1 still holds and is still tested; it
//!    is no longer load-bearing for liveness, which is where a rule that can only be *stated* in a
//!    shipped build should not be left.
//! 2. **A build publishes only into its own slot,** identified by [`Slot::Building`]'s `seq` and
//!    not merely by its state. Without the sequence number, a prune landing mid-build is silently
//!    undone by the publish that follows it, and — the sharper failure — a build that unwinds
//!    after a prune-and-reinsert deletes a *different* builder's slot. Both paths compare the
//!    sequence: the publish below, and [`RemoveOnUnwind`]. **This closes a hole that is
//!    unreachable today**, because nothing currently removes a slot between the `Building` insert
//!    and the guard firing; `retain_keys` is what makes it reachable, so the guard is hardened in
//!    the same change that arms it rather than afterwards.
//! 3. **The building caller always receives what it built**, retained or not. The value is
//!    returned from the build, never re-read out of the map. This is what makes forward progress
//!    structural rather than a property of the eviction order, and it is the whole of the
//!    undersized-bound guarantee: a bound below the working set costs rebuilds, never a refusal.
//! 4. **Evicted `Arc`s are collected under the lock and dropped after it is released.** Dropping a
//!    ~125 MB bitmap frees ~15 k containers; doing that inside a lock whose O(1) hold time is
//!    load-bearing convoys the branch's 48 admitted requests. Every removal path funnels into
//!    [`Slots::remove`], which hands the value back rather than dropping it, and the caller drops
//!    the collected `Vec` after the guard. [`SingleFlightCache::is_locked_now`] makes this
//!    testable rather than merely stated — and it is asserted on **both** removal paths: the
//!    eviction pass inside a publish, and [`SingleFlightCache::retain_keys`], which is the path the
//!    request-path pruners take and which frees whole sessions' worth of bitmaps at once.
//!
//!    This is the one rule with no type behind it: the `let mut dead` declared before the guard is
//!    a convention, replicated in three functions here and three more in the authz twin. What
//!    enforces it beyond the tests is that [`Slots::remove`] is `#[must_use]`, so the natural
//!    lapse — `for key in doomed { slots.remove(key); }`, dropping the value in place — is a
//!    compile error under this workspace's `-D warnings`, in both crates and in functions not yet
//!    written. A `Deferred<V>` newtype returned out of each locked block was considered and
//!    declined: it moves the same convention into a type whose `Drop` is still the thing that must
//!    not run under the lock, buying a name rather than an enforcement.
//!
//! # Rule 5: waiting, and why a wake cannot be missed
//!
//! 5. **Every exit from `Building` notifies that slot's waiters, and a waiter decides by
//!    re-reading the map rather than by trusting the wake.** A waiter that is never woken is a
//!    hang, which is strictly worse than the refusal it replaces, so the argument has to be
//!    structural rather than a list of call sites that happened to be found.
//!
//!    `Building` is left by four routes — a successful publish, the publish's oversized arm, a
//!    build that unwinds ([`RemoveOnUnwind`]), and a prune ([`SingleFlightCache::retain_keys`])
//!    landing mid-build — but they are only **two writes**. Three of them remove the slot, and
//!    every removal funnels through [`Slots::remove`], which is rule 4's choke point already;
//!    the fourth overwrites it, in the single insertion site inside [`SingleFlightCache::publish`].
//!    Notifying at those two points covers all four by construction, and any fifth route would have
//!    to go through one of them to exist at all. Both notify while holding the map lock: the woken
//!    threads re-block on the mutex until it is released, which costs a scheduling round-trip and
//!    buys the guarantee that no wake can be issued between a waiter's decision to sleep and its
//!    sleeping.
//!
//!    **The condvar is per slot, not per cache.** Many condvars against one mutex is the legal
//!    direction (one condvar against two mutexes is not), so this costs a word per `Building` slot
//!    and avoids waking every waiter in the map on every publish — which would preserve F4's
//!    property on paper while reintroducing its symptom.
//!
//!    **A waiter carries `seq` into the wait and re-checks it on wake** (rule 2, applied to a third
//!    party): the slot it is woken for may already be a *later* builder's under the same key, and
//!    being satisfied by that one is the same class of error as publishing into it. On any exit
//!    that is not its own build's `Ready`, a waiter falls back to a plain **miss** — it takes the
//!    slot and builds — rather than to an error, because a panicked, oversized or pruned build is
//!    exactly the state a fresh arrival would find, and returning `Building` there would reinstate
//!    the refusal this rule exists to remove. The wait budget, not the number of rounds, is what
//!    bounds that loop.
//!
//! # What the bound bounds, and what it does not
//!
//! `bound_bytes` bounds the bytes **resident in this map**. It is not a bound on process memory,
//! and eviction is what creates the difference: every in-flight caller holds an `Arc<V>` for the
//! duration of its request whether or not the map still contains it, so the peak is
//! `bound_bytes + admission_width × per_entry`. An operator sizing a box from the config key alone
//! will under-provision; the arithmetic is stated with its measured per-entry figure on
//! [`crate::cache::RowProjectionCache`].
//!
//! # The duplicated twin, and which test covers which rule in which crate
//!
//! `tessera_authz::single_flight` is a near-copy of this module. It cannot reuse it: `tessera-authz`
//! sits *below* this crate in the graph and `scripts/check-layers.sh` enforces that direction, and
//! the two use sites need different signatures anyway (that copy's build is `io::Result`, so it
//! carries `get_or_try_build`). Every one of the rules below can therefore be right in one crate and
//! wrong in the other, and **a test written once covers only the crate it lives in** — the round-1
//! review found the authz guard's `seq` comparison could be reverted with that crate's suite still
//! green, precisely because its twin test was missing.
//!
//! This table is the cheap durable fix: it names, per rule, the test in each crate, so a missing
//! twin shows up by reading rather than by mutation testing. The authz copy carries the same table.
//!
//! | Rule / property | this crate | `tessera-authz` |
//! |---|---|---|
//! | 1 — `Building` is never a victim | `a_building_slot_is_never_evicted` | same name |
//! | 1 — the eviction loop terminates regardless | `an_undersized_bound_does_not_livelock` | same name |
//! | 2 — the publish compares `seq` | `a_prune_during_a_build_is_not_undone_by_the_publish` | `an_evict_during_a_build_is_not_undone_by_the_publish` |
//! | 2 — the unwind guard compares `seq` | `an_unwinding_build_does_not_delete_a_later_builders_slot` | `a_failed_build_does_not_delete_a_later_builders_slot` |
//! | 3 — the builder receives what it built | `an_entry_larger_than_the_bound_is_served_but_not_retained`, `an_undersized_bound_does_not_livelock` | same two names |
//! | 4 — evicted values drop outside the lock | `evicted_arcs_are_dropped_outside_the_lock` | same name |
//! | 4 — on the bulk-removal path too | `pruned_arcs_are_dropped_outside_the_lock` | `evicted_arcs_from_evict_are_dropped_outside_the_lock` |
//! | the floor turns a byte bound into an entry bound | `tiny_entries_are_charged_the_floor`, the `const _` below | same name, the `const _` there |
//! | the counted choke point | `a_hit_takes_exactly_one_lock`, `a_miss_takes_exactly_two_locks` | `a_hit_takes_one_lock_and_a_miss_takes_two` |
//! | LRU order, not insertion order | `eviction_takes_the_least_recently_used` | same name |
//! | `young_evictions` is the thrash alarm | `young_evictions_counts_only_never_reused_entries` | same name |
//! | single-flight, non-blocking waiters, panic safety | `a_ready_hit_never_calls_build_again`, `concurrent_miss_…`, `distinct_keys_…`, `a_panicking_build_…` | same four names |
//! | 5 — a waiter is served the winner's value, built once | `a_waiter_is_served_the_winners_value_and_the_build_runs_once` | — (no waiting entry point there) |
//! | 5 — the unwind wake, and the waiter does not inherit the panic | `a_waiter_whose_build_panics_falls_back_to_building_it` | — |
//! | 5 — the oversized-publish wake | `a_waiter_whose_build_is_oversized_falls_back_to_building_it` | — |
//! | 5 — the prune wake | `a_waiter_whose_build_is_pruned_falls_back_to_building_it` | — |
//! | 5 — a later builder's value satisfies a waiter, and `seq` is deliberately absent from the wait | `a_waiter_takes_a_later_builders_value_for_the_same_key` | — |
//! | 5 — the budget bounds the wait | `a_waiter_that_is_never_satisfied_times_out_as_building` | — |
//! | 5 — cancellation releases before the budget | `a_cancelled_waiter_releases_before_the_budget` | — |
//! | 5 — the non-waiting entry point still refuses | `concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild` | same name |
//!
//! **Three deliberate asymmetries**, listed so they are not read as gaps: bulk pruning
//! ([`SingleFlightCache::retain_keys`], `retain_keys_removes_only_rejected_keys_in_one_acquisition`)
//! exists only here, single-key `evict` (`evict_removes_exactly_one_key`) only there, and the `Err`
//! form of a failed build (`a_failed_build_leaves_the_key_absent_so_a_retry_rebuilds`) only there,
//! because this cache's build is infallible.
//!
//! Generic over `K`/`V` and free of any `RowProjection`/`Engine` knowledge, so the state machine
//! itself is unit-testable here with a trivial `V` and a controllable `build` closure, without
//! opening a bundle — see this module's tests for the deterministic (channel-synchronised, not
//! timing-dependent) reproductions of single-flight, non-blocking-waiter, panic-safety, eviction
//! and lock-discipline behaviour.

use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use crate::cancel::CancelToken;

/// What one cached value costs the byte bound.
///
/// A trait rather than a `fn(&V) -> u64` handed to the constructor: the size of a value is a
/// property of its type, and a function pointer inside the cache's own type signature buys nothing
/// and reads worse. Implemented for `RowProjection` in [`crate::cache`].
pub(crate) trait CacheWeight {
    /// This value's contribution to the byte bound, **before** the per-entry floor is applied.
    ///
    /// Must be cheap, and is computed once per successful build, **outside** the lock.
    fn cache_weight_bytes(&self) -> u64;
}

/// The minimum a cached entry is charged, whatever [`CacheWeight`] reports — **modelled, not
/// measured**.
///
/// **Why a floor exists at all: a byte bound alone does not bound entry count.** A
/// zero-cardinality projection serialises to a handful of bytes, and `Engine::authorise` mints a
/// fresh `token_id` on every call against an already-cached fragment, so a caller can insert
/// unbounded entries that never trip a byte bound while each costs hundreds of bytes of real
/// memory. **This is the second appearance of that shape**: the same
/// session-rotation bypass defeats lifecycle §2.2's per-session pin cap (see
/// `crate::pins::PinManager::pins_per_session_max`, which records that rotation is free because
/// `authorise` mints a `token_id` per call against a cached fragment). A third reader meeting it
/// should recognise it rather than rediscover it. Charging `max(weight, floor)` makes the byte
/// bound imply an entry ceiling of `bound / floor`, so one mechanism bounds both.
///
/// **512 B is MODELLED, and this project does not blur modelled with measured.** There is no
/// measurement behind it and it must not be read as one. The inventory it is built from, per
/// entry: the `Arc<K>` allocation and its two refcounts (~56 B with a `String` key), the key's own
/// heap buffer (~16–32 B for a slice name), a hashbrown slot (~40 B), the `BTreeMap` node's
/// amortised share (~24 B), the `Arc<V>` allocation (~40 B), and croaring's `roaring_bitmap_t`
/// with its container array — opaque C-side allocation that `size_of` cannot see at all. That
/// inventory lands at ~190–260 B; 512 B is the next round number above it, chosen so the charge
/// stays an **over**-estimate. Over-charging is the fail-safe direction for a memory bound (it
/// evicts sooner than strictly necessary); under-charging is what makes a bound stop bounding.
///
/// Deliberately *not* derived from `size_of` — the assertion below only checks it cannot become an
/// under-charge. A derivation would be a confident under-estimate, because the largest opaque term
/// is on the C side where `size_of` has no visibility.
pub(crate) const PER_ENTRY_FLOOR_BYTES: u64 = 512;

/// One key's state.
///
/// There is deliberately no third, "failed" state: D-G requires a failed (panicking) build to
/// remove the entry outright rather than cache anything for it, so the next arrival retries —
/// caching a failure would be a permanent fail-closed wedge (I13a).
enum Slot<K, V> {
    /// A build is in flight. `seq` identifies *which* build, so a publish or an unwind can tell
    /// its own slot from a later builder's — this module's doc, rule 2.
    ///
    /// Carries no value, is charged no bytes, and is absent from the recency index (rule 1).
    ///
    /// **`wake` is this slot's own condvar, and callers park on it** (rule 5): a caller through
    /// [`SingleFlightCache::get_or_derive_waiting`] sleeps here until this build resolves, rather
    /// than being refused. Per slot rather than per cache so a publish wakes the callers waiting
    /// for *that* key and no others. It is notified by whichever write displaces this variant —
    /// [`Slots::remove`] or the insert in [`SingleFlightCache::publish`] — and those are the only
    /// two, which is the whole of rule 5's no-missed-wake argument.
    Building { seq: u64, wake: Arc<Condvar> },
    Ready {
        value: Arc<V>,
        /// The map's own key, held here for two distinct reasons that an earlier draft of this
        /// comment ran together — it claimed only the second, and the code did not deliver it until
        /// the round-1 fixes.
        ///
        /// 1. **No `K: Clone` on the hot path.** Re-inserting into the recency index needs an owned
        ///    key; without this field a hit would clone `K` itself (a `String` allocation for
        ///    `RowProjectionKey`'s slice name) on every warm request. Cloning the `Arc` is a
        ///    refcount bump. This is what the field really buys, and it is worth its one word.
        /// 2. **One hash lookup per hit.** A touch needs both the `Arc<K>` and `&mut Slot`, and
        ///    `get_key_value` + `get_mut` would be two. This *is* now true — [`Slot::Ready`]'s
        ///    `tick` is stamped inside the same `get_mut` arm that reads the value — but it was not
        ///    while the new tick was written through a second `get_mut` after the first borrow
        ///    ended, which is exactly what the code did when this comment first asserted it.
        key: Arc<K>,
        /// `max(weight, PER_ENTRY_FLOOR_BYTES)` — what this entry contributes to `bytes`. Stored
        /// rather than recomputed, so removal can never disagree with insertion about how much to
        /// give back; that disagreement is how a byte accounting drifts.
        charged: u64,
        /// This entry's key in [`Slots::recency`]. Ticks are unique and monotonic for the life of
        /// the cache, which is what makes this a bijection with the index.
        tick: u64,
        /// Hits since publication — `0` at the moment it is published. An eviction at `0` is an
        /// entry that was never reused, the thrash signature [`CacheStats::young_evictions`]
        /// exists to report.
        uses: u32,
    },
}

/// A losing arrival's outcome: some other caller is already building this key, and this call did
/// not wait for it. Carries nothing — the caller only needs to know to retry, never a handle to
/// the in-flight build.
#[derive(Debug)]
pub(crate) struct Building;

/// Why a waiting caller ([`SingleFlightCache::get_or_derive_waiting`]) gave up. Both are refusals,
/// and they are kept apart because the server owes the client different answers: a budget
/// expiry is the 429 that used to be immediate, a cancellation is the client's own disconnect and
/// must not be reported as backpressure.
///
/// There is deliberately no "the build failed" variant. A build that panics, is oversized or is
/// pruned leaves the waiter looking at a plain miss, which it then builds itself — rule 5. Giving
/// that its own outcome would hand the caller an error for a state a fresh arrival handles without
/// one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitEnded {
    /// The wait budget (`serve.single_flight_wait_ms`) expired with no value to serve.
    Budget,
    /// The request's [`CancelToken`] was flipped — the client is gone.
    Cancelled,
}

/// A waiting caller's two bounds, carried together so neither can be supplied without the other.
#[derive(Clone, Copy)]
struct Wait<'a> {
    cancel: &'a CancelToken,
    deadline: Instant,
}

/// How long a parked waiter sleeps before re-reading its [`CancelToken`]. See
/// [`SingleFlightCache::park`] for why cancellation polls rather than wakes.
const WAIT_TICK: Duration = Duration::from_millis(50);

/// The wait budget an embedder that never calls [`SingleFlightCache::set_wait_budget_ms`] gets.
///
/// **Argued from the build it has to outlast, not chosen for roundness.** A full row-projection
/// rebuild at 10⁹ is a *measured* 1 277 ms (`crate::refresh`'s table), and a racer can arrive at
/// any point during one, so any budget at or below that reproduces decision 0058's defect at the
/// scale that motivated it. 6,000 ms is that figure with headroom for a loaded box. It is
/// deliberately **not** inherited from the compute gate's `admission_timeout_ms` (250 ms), which
/// bounds queueing for a permit rather than the work a permit is held for, nor from the client's
/// `Retry-After: 1`.
///
/// The operator's knob is `serve.single_flight_wait_ms`; decision 0059 records why the slot
/// occupancy this admits is bounded here rather than by a per-principal cap.
pub const DEFAULT_WAIT_BUDGET_MS: u64 = 6_000;

/// What [`SingleFlightCache::peek`] found — a read that claims nothing.
///
/// The three states are distinguished because a caller under decision 0044 answers them
/// differently: `Building` means a producer exists and the caller should fall back rather than
/// start a second one; `Absent` means nothing is coming and the caller may build.
pub(crate) enum Peek<V> {
    Ready(Arc<V>),
    Building,
    Absent,
}

/// Operator-facing cache gauges. Every field is read from an atomic **without taking the slot
/// lock**, deliberately, so that [`Self::slot_locks`] stays an honest measure of this type's own
/// locking rather than of the caller's polling — the same discipline `crate::pins::PinStats` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Slots currently held, `Building` and `Ready` both counted.
    pub entries: usize,
    /// **Charged** bytes currently resident: `Σ max(weight, PER_ENTRY_FLOOR_BYTES)` over `Ready`
    /// slots. Deliberately not "resident bytes" — the floor makes this an over-estimate for small
    /// entries, and an operator diffing it against RSS should know which way it errs. It is also
    /// not process memory; see this module's doc.
    pub bytes: u64,
    /// The configured byte bound, carried so an alarm on `bytes` needs no second input.
    pub bound_bytes: u64,
    pub hits: u64,
    /// Builds started. A losing arrival that gets [`Building`] is neither a hit nor a miss: it
    /// neither read a value nor built one.
    pub misses: u64,
    /// Callers turned away because a build was already in flight on their key. **This is the 429
    /// rate**, and it is bounded by *same-key* concurrency — a working set that does not fit
    /// produces rebuilds, not refusals (rule 3).
    ///
    /// **Its population changed with decision 0058 and its name did not.** It used to count
    /// *races*: every arrival that found a `Building` slot. It now counts only the two arrivals
    /// that still refuse — one through the non-waiting entry point (`crate::refresh`), and one
    /// whose wait budget expired. A racer that waits and is served is counted by
    /// [`Self::waits_satisfied`] instead, so the two together are the old figure, and reading this
    /// one alone as a race rate now understates it.
    pub building_refusals: u64,
    /// Waits that ended with the winner's value — the 429s decision 0058 converted into answers.
    ///
    /// Against [`Self::building_refusals`] this is the whole picture of same-key contention: sum
    /// for the race rate, ratio for whether the budget is set sensibly. A waiter that fell through
    /// to building its own value (its build panicked, was oversized, or was pruned) is in neither
    /// figure — it is a [`Self::misses`], because that is what it became.
    pub waits_satisfied: u64,
    /// Callers parked in a wait **at this instant** — a gauge, not a total.
    ///
    /// What it is for: a parked caller holds a `ComputeGate` permit while burning no CPU, and that
    /// occupancy is the price decision 0058 pays and decision 0059 declines to bound with a
    /// per-principal cap. Sustained non-zero here against a low `waits_satisfied` is a budget set
    /// too high.
    ///
    /// **It does not attribute, and must not be read as though it did.** It is process-wide: it
    /// says whether waiting occupies the gate, never whose waiting does. Deciding that one
    /// principal is the cause needs per-auth-hash instrumentation, which SA §9 keeps off this
    /// surface.
    pub waiters_now: u64,
    pub evictions: u64,
    pub evicted_bytes: u64,
    /// Evictions of entries that were never reused (`uses == 0`).
    ///
    /// **The thrash alarm, given a definition.** `evictions` alone cannot distinguish healthy
    /// eviction of genuinely cold entries from a working set that does not fit — both produce
    /// evictions at the same rate. An entry evicted before it was ever hit again is the signature
    /// of the second. A sustained non-zero rate here is the alarm; sustained zero with non-zero
    /// `evictions` is a cache doing its job.
    pub young_evictions: u64,
    /// Builds whose value alone exceeded the whole bound: served to their caller, never retained.
    ///
    /// Emptying the cache to admit one entry it still could not hold would be the worst available
    /// response to a single pathological grant set, so the entry is simply not admitted.
    pub oversized_admissions: u64,
    /// **Every** acquisition of the slot mutex since construction, from anywhere in this module.
    ///
    /// This is the counter the lock-discipline tests assert on, and it measures a property of the
    /// type rather than of one call path *because* there is exactly one place in this file that
    /// locks ([`SingleFlightCache::lock_slots`]). Any new lock acquisition added here must come
    /// through it, or those tests silently stop covering the property.
    ///
    /// One deliberate exception, named so this doc does not claim more than the code delivers:
    /// [`SingleFlightCache::is_locked_now`] is a `try_lock` and is **not** counted. It exists only
    /// under `cfg(test)`, and counting it would perturb the exact-count assertions it is used
    /// alongside.
    pub slot_locks: u64,
    /// Entries walked by [`SingleFlightCache::retain_keys`] passes, cumulatively. A prune is O(n)
    /// under the request-path lock, so this is what makes the pruners' cost observable rather than
    /// argued — see `RowProjectionCache::prune_token` for the arithmetic and the threat model.
    pub prune_scanned: u64,
}

/// The guarded state. One struct rather than several `Mutex`es so the map, the recency index and
/// the byte accounting cannot be updated apart — see [`Slots::remove`].
struct Slots<K, V> {
    map: FxHashMap<Arc<K>, Slot<K, V>>,
    /// `Ready` entries only, ordered oldest-use first. **`Building` slots are deliberately absent**
    /// (this module's doc, rule 1), which keeps `recency.len()` equal to the number of `Ready`
    /// slots and lets eviction take the front unconditionally.
    ///
    /// A `BTreeMap` keyed by a monotonic tick rather than an intrusive doubly-linked list over a
    /// slab: a touch is `remove(old) + insert(new)`, two O(log n) operations and an `Arc` refcount
    /// bump, against O(1) for the list. The list is faster and is manual `Option<usize>` pointer
    /// surgery with a free list; at ~150–250 ns against ~40 ns, on a request measured in
    /// milliseconds, CLAUDE.md's "prefer the construction that is obviously correct" decides it.
    /// Recorded so a later performance case can be made against a stated baseline rather than
    /// rediscovered.
    ///
    /// The refused third option was `last_used: u64` per slot with a sort at eviction time: one
    /// store on the hot path, but O(n log n) per pass under the request-path lock, and the floor
    /// permits n up to `bound / 512` — 4.2 M at the 2 GiB default, i.e. a sub-second pass while
    /// every admitted request blocks.
    recency: BTreeMap<u64, Arc<K>>,
    /// Monotonic and never reused for the life of the cache. Uniqueness is what makes
    /// `Slot::Ready`'s `tick` a bijection with `recency`; reuse would let a touch remove another
    /// key's index entry, after which eviction evicts a hot entry and a cold one becomes immortal.
    next_tick: u64,
    /// Monotonic build identity — see [`Slot::Building`]'s `seq`.
    next_seq: u64,
    /// `Σ charged` over `Ready` slots. Mirrors [`CacheStats::bytes`].
    bytes: u64,
}

impl<K: Eq + Hash, V> Slots<K, V> {
    /// **The one place a slot is removed.** Every removal path funnels here: eviction, pruning, the
    /// publish's oversized arm, and the unwind guard.
    ///
    /// **It is not the only place all three structures are written** — an earlier draft of this doc
    /// claimed it was "the one place the map, the recency index and `bytes` could go out of step",
    /// and [`SingleFlightCache::publish`] disproves it by updating all three by hand on the
    /// insertion side. The honest claim is narrower and is the one that matters: removal is the
    /// direction with four callers, insertion has exactly one, and this is the same "one choke
    /// point" argument [`SingleFlightCache::lock_slots`] makes for locking, applied to the
    /// direction that most needs it.
    ///
    /// Returns the removed value **rather than dropping it**, so the caller can drop it after
    /// releasing the lock (this module's doc, rule 4). A `Building` slot has no value and returns
    /// `None` while still being removed.
    ///
    /// `#[must_use]` is rule 4's only compile-time enforcement. The lapse it refuses is
    /// `for key in doomed { slots.remove(key); }` — the natural shape for a pruner someone adds
    /// later — which drops n × ~125 MB of bitmaps, ~15 k containers each, inside the request-path
    /// critical section. A caller that genuinely means to discard the value must say so with a
    /// binding and a reason; the two that do are in the oversized-publish arm and the unwind guard,
    /// and both remove a `Building` slot, which carries no value at all.
    #[must_use = "rule 4: the removed value must be carried out of the critical section and dropped \
                  after the lock is released, never dropped in place"]
    fn remove(&mut self, key: &K) -> Option<Arc<V>> {
        match self.map.remove(key) {
            None => None,
            // **Rule 5's removal half.** Three of the four exits from `Building` are removals and
            // all three arrive here, so waking once here covers the oversized-publish arm, the
            // unwind guard and a prune landing mid-build without any of them knowing waiters
            // exist. The wake is issued with the map lock held — this runs inside a locked block
            // in every caller — so it cannot land in the window between a waiter deciding to sleep
            // and sleeping. Each woken caller re-reads the map, finds its build's slot gone, and
            // becomes a builder itself.
            Some(Slot::Building { wake, .. }) => {
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
    /// A `debug_assert`-shaped check rather than a release one — it is O(n) — but it runs in every
    /// test in this file and in every test that drives a real cache, which is where a bijection
    /// break would otherwise stay invisible until it had silently removed the bound.
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
/// concurrency shape and the four eviction rules.
pub(crate) struct SingleFlightCache<K, V> {
    slots: Mutex<Slots<K, V>>,
    /// Read on every publish, and settable after construction because the value comes from
    /// `tessera-server`'s config while the cache is built inside `Engine::open`, which cannot see
    /// it. The config-field route through `EngineConfig` is closed by this stage's frozen test
    /// files — the same constraint `Engine::start_write_executor` records for `ingest_queue_bound`.
    bound_bytes: AtomicU64,
    /// The wait budget for [`SingleFlightCache::get_or_derive_waiting`], in milliseconds. Settable
    /// after construction for the same reason `bound_bytes` is, and by the same route.
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
    /// `bound_bytes` is the byte ceiling on resident entries. `u64::MAX` means "no bound", which is
    /// unbounded, and what every non-server construction site gets; `tessera-server`
    /// always sets a real one at startup, after validating it (`tessera_server::prepare`).
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

    /// Set the byte bound. Called once at startup, before any request is served; setting it under
    /// load is sound but takes effect only at the next publish.
    pub(crate) fn set_bound_bytes(&self, bound_bytes: u64) {
        self.bound_bytes.store(bound_bytes, Ordering::Relaxed);
    }

    /// Set the wait budget. Read once per waiting call, at entry, so a change under load takes
    /// effect for calls that arrive after it and never shortens a wait already in progress.
    pub(crate) fn set_wait_budget_ms(&self, wait_budget_ms: u64) {
        self.wait_budget_ms.store(wait_budget_ms, Ordering::Relaxed);
    }

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic, not a capacity
    /// bound. `Engine::row_projection_cache_len` publishes this, and it is the observable behind
    /// "`Engine::item` must never construct a projection, warm or cold", which counts slots in
    /// either state.
    pub(crate) fn len(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// The operator gauges — see [`CacheStats`]. Lock-free, deliberately.
    pub(crate) fn stats(&self) -> CacheStats {
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

    /// Look up `key` **without waiting** for a build already in flight.
    ///
    /// A hit clones the `Arc`, moves the entry to the young end of the recency order and returns
    /// without calling `make` at all. A miss makes this call the builder: publish `Building`, drop
    /// the lock, run `make` outside it, re-lock, evict to fit, publish `Ready`. A *different*
    /// concurrent miss on the same key observed while this is in flight gets `Err(Building)`
    /// immediately.
    ///
    /// **This is now the exception rather than the rule** (decision 0058): the request path takes
    /// [`Self::get_or_derive_waiting`], and this entry point exists for the caller that must not
    /// block — `crate::refresh`, which runs on a rayon worker and would park it behind a build
    /// that itself needs the pool. That call site carries the argument; see also this module's doc.
    ///
    /// **Exactly two lock acquisitions on a miss, exactly one on a hit**, asserted by this module's
    /// tests against [`CacheStats::slot_locks`]. The LRU touch and the eviction pass both happen
    /// *inside* those acquisitions and add none of their own; a recency index behind its own mutex,
    /// or a re-lock to evict after publishing, is what those assertions refuse.
    ///
    /// **Panic safety (I13a).** If `build` unwinds, [`RemoveOnUnwind`] removes *this build's*
    /// `Building` entry — identified by its sequence number, so a slot a later builder owns is left
    /// alone — before the unwind propagates, and the next arrival sees a plain miss.
    ///
    /// **Rule 3: the value is returned whether or not it was retained.** An eviction that cannot
    /// make room, or a value larger than the whole bound, costs a future rebuild and never this
    /// caller's answer.
    ///
    /// **Invariant: `build` must be infallible.** `impl FnOnce() -> V` has no way to signal
    /// failure, so a build that can fail must not be wrapped in a closure that panics or that
    /// stuffs an error into `V` — use `tessera-authz`'s `SingleFlightCache::get_or_try_build` twin
    /// instead, which carries the `Result` through the slot state machine properly (fail-closed,
    /// not a cached failure — I13a).
    /// Look up `key`, with the chance to build a miss's value **from another key's entry**.
    ///
    /// `make` is handed `Some(source)` when `derive_from` names a key that is `Ready` at the moment
    /// this call claims its own slot, and `None` otherwise. Everything else — the single-flight
    /// state machine, the two lock acquisitions, the panic safety, rule 3 — is identical, because
    /// this is the one entry point.
    ///
    /// **`derive_from` is a hint and must be treated as one.** A source that has been evicted, is
    /// still building, or was never inserted yields `None`, and `make` must then produce exactly the
    /// value it would have produced from scratch. That is what keeps a derived entry
    /// indistinguishable from a built one — the property `crate::cache`'s own doc rests on, and the
    /// reason this takes one closure with an `Option` rather than two closures: two closures are two
    /// places for the answers to diverge.
    ///
    /// **The source is read inside the same acquisition that claims the target slot**, so it cannot
    /// be evicted between the decision to derive and the derivation. It is `Arc`-cloned, so the
    /// derivation itself runs outside the lock like any other build, and the source entry is
    /// **not** touched for recency: deriving from an entry is not a use of it, and counting it as
    /// one would keep a superseded generation's entries young for as long as anything derived from
    /// them.
    pub(crate) fn get_or_derive(
        &self,
        key: K,
        derive_from: Option<&K>,
        make: impl FnOnce(Option<&V>) -> V,
    ) -> Result<Arc<V>, Building> {
        self.claim_and_build(key, derive_from, None, make)
            .map_err(|_ended| Building)
    }

    /// Look up `key`, **waiting** for a build already in flight rather than refusing (decision
    /// 0058). Everything else — the state machine, the panic safety, rule 3, `derive_from`'s
    /// treatment — is [`Self::get_or_derive`]'s, because both are one function.
    ///
    /// Three ways out for a caller that finds a build in flight:
    ///
    /// - the build publishes → this returns its value, and `make` never runs here;
    /// - the build **fails to publish** — it panicked, it was oversized, or a prune removed its
    ///   slot — → this caller takes the slot and builds, exactly as a fresh arrival would;
    /// - the wait budget expires, or `cancel` is flipped → `Err`, which the request path turns
    ///   into the 429 that used to be immediate, or into `EngineError::Cancelled`.
    ///
    /// **The budget is what bounds the wedge** that this module's `Slot` doc names: a build that
    /// dies without going through either notify path would otherwise leave waiters asleep for
    /// ever. Rule 5 argues no such path exists; the budget is what makes that argument's failure a
    /// bounded stall rather than a hang, and it is a condition of 0058 rather than a tuning knob.
    ///
    /// **`cancel` is polled on a tick rather than waking the waiter** ([`WAIT_TICK`]). A
    /// disconnected client therefore releases within a tick instead of instantly, which is the
    /// price of not threading a second wakeup path through [`crate::cancel::CancelToken`] — a
    /// cross-thread notify would need every token to know which condvars to hit, for a saving
    /// measured in tens of milliseconds against a budget measured in seconds.
    pub(crate) fn get_or_derive_waiting(
        &self,
        key: K,
        derive_from: Option<&K>,
        cancel: &CancelToken,
        make: impl FnOnce(Option<&V>) -> V,
    ) -> Result<Arc<V>, WaitEnded> {
        // Taken once, here, rather than per park: the budget bounds the whole call, so a waiter
        // that is woken, finds *another* builder and parks again cannot renew it. That is what
        // makes a succession of short-lived builders bounded rather than a livelock.
        let deadline =
            Instant::now() + Duration::from_millis(self.wait_budget_ms.load(Ordering::Relaxed));
        self.claim_and_build(key, derive_from, Some(Wait { cancel, deadline }), make)
    }

    /// The one body behind both entry points. `wait` present means "wait for an in-flight build";
    /// absent means "refuse".
    fn claim_and_build(
        &self,
        key: K,
        derive_from: Option<&K>,
        wait: Option<Wait<'_>>,
        make: impl FnOnce(Option<&V>) -> V,
    ) -> Result<Arc<V>, WaitEnded> {
        // Declared before any lock guard so that, whatever path is taken below, these drop AFTER
        // the guard goes out of scope (Rust drops locals in reverse declaration order) — rule 4.
        // Every locked block below writes evicted values here rather than dropping them in place.
        let mut dead: Vec<Arc<V>> = Vec::new();

        let (owned_key, seq, source) = {
            let mut slots = self.lock_slots();
            // Set by the first park, and read only to count a wait that ended in the winner's
            // value — a waiter that falls through to building its own is not a satisfied wait.
            let mut waited = false;
            loop {
                // Read before the `get_mut` borrow opens, so the whole touch — bumping `uses`,
                // stamping the new tick — happens inside the one lookup. Consumed only on the hit
                // branch; a tick read and not used costs nothing, because only uniqueness and
                // monotonicity matter, never density.
                let new_tick = slots.next_tick;
                let hit = match slots.map.get_mut(&key) {
                    None => None,
                    Some(Slot::Building { wake, .. }) => {
                        let Some(wait) = wait else {
                            self.building_refusals.fetch_add(1, Ordering::Relaxed);
                            return Err(WaitEnded::Budget);
                        };
                        let wake = Arc::clone(wake);
                        // `?` returns the budget or cancellation answer; `Ok` means "woken, decide
                        // again from what the map says now" — which is the loop, and which is also
                        // how a spurious wakeup is absorbed without either returning early or
                        // renewing the budget.
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
                // Read under the same lock that is about to claim this key's slot. Deliberately
                // *before* the miss branch inserts `Building`, and deliberately not a recency
                // touch — see this method's doc.
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
        // An unwinding `build` therefore always leaves this build's key absent rather than stuck at
        // `Building` — and, because the guard compares `seq`, never removes a later builder's slot.
        let mut guard = RemoveOnUnwind {
            cache: self,
            key: Arc::clone(&owned_key),
            seq,
            disarmed: false,
        };

        let value = Arc::new(make(source.as_deref()));

        // **Outside the lock, deliberately.** `get_serialized_size_in_bytes` is O(containers) —
        // ~15 k at the 125 MB operating point — and computing it inside the critical section would
        // quietly end the O(1)-hold-time property this module's whole design rests on.
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

    /// Sleep on one `Building` slot's condvar until something displaces it, the budget runs out, or
    /// the caller's request is cancelled. Returns the re-acquired guard, and the caller re-reads
    /// the map — a wake is never treated as "ready".
    ///
    /// **Deliberately not `seq`-aware, and this is the one place that departs from rule 2's
    /// habit.** A publish and an unwind must compare `seq` because writing into another build's
    /// slot corrupts it. A waiter only *reads*, and the key wholly determines the value here (it
    /// is what [`Self::peek`] rests on too), so a `Ready` left by a later builder under the same
    /// key is exactly as correct an answer as this build's would have been — refusing it would
    /// force a rebuild of a value already in the map. Carrying `seq` into the wait was specified
    /// and is not built; nothing needs it, because the decision is taken from the map's present
    /// state under the lock rather than from anything remembered across the sleep.
    ///
    /// **The tick, and why it is not the budget.** `wait_timeout` is capped at [`WAIT_TICK`] so
    /// [`CancelToken`] — which has no wakeup path of its own — is re-read that often. Cost at the
    /// gate's 48 admitted requests is ~960 re-acquisitions a second of a mutex held for O(1),
    /// against a budget in seconds; F4's property is about hold time, which this does not touch.
    fn park<'a>(
        &self,
        slots: MutexGuard<'a, Slots<K, V>>,
        wake: &Condvar,
        wait: Wait<'_>,
    ) -> Result<MutexGuard<'a, Slots<K, V>>, WaitEnded> {
        // Both checked before sleeping, so a caller arriving with an already-expired budget or an
        // already-cancelled request never parks at all.
        if wait.cancel.is_cancelled() {
            return Err(WaitEnded::Cancelled);
        }
        let remaining = wait.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.building_refusals.fetch_add(1, Ordering::Relaxed);
            return Err(WaitEnded::Budget);
        }

        // The gauge decision 0059 ships: parked callers hold a `ComputeGate` permit while burning
        // no CPU, and that occupancy is the cost of waiting. Process-wide and unattributed — it
        // says whether waiting occupies the gate, never *whose* waiting does, which is exactly the
        // per-principal state that decision declines to keep.
        self.waiters_now.fetch_add(1, Ordering::Relaxed);
        // **Not counted through `lock_slots`, so counted here.** `wait_timeout` re-acquires the
        // mutex without passing the choke point, and leaving it uncounted would make
        // `CacheStats::slot_locks` quietly understate acquisitions the moment anyone waits. The
        // exact-count tests take the non-waiting path and are unaffected either way.
        self.slot_locks.fetch_add(1, Ordering::Relaxed);
        let (slots, _timed_out) = wake
            .wait_timeout(slots, remaining.min(WAIT_TICK))
            .unwrap_or_else(PoisonError::into_inner);
        self.waiters_now.fetch_sub(1, Ordering::Relaxed);

        // `_timed_out` is deliberately unread: the tick expiring means "re-check the token", not
        // "give up", and the budget is re-tested at the top of the next park. Branching on it here
        // would end the wait at the first tick.
        Ok(slots)
    }

    /// The publish half of a miss, factored out so the lock scope above stays readable. Runs with
    /// the lock held.
    ///
    /// Three outcomes, and the first two are the ones a natural implementation gets wrong:
    ///
    /// - **This build's slot is gone** (a prune removed it) **or carries another build's `seq`** →
    ///   publish nothing. Re-inserting would silently undo the prune.
    /// - **The value alone exceeds the whole bound** → remove this build's `Building` slot and
    ///   publish nothing. **Removing it is essential, not tidiness**: a `Building` slot with no
    ///   builder is a permanent `ProjectionBuilding` wedge for that key — an unrecoverable 429 for
    ///   the rest of that session — which is exactly the fail-closed wedge [`Slot`]'s own doc says
    ///   the two-state design exists to prevent (I13a).
    /// - otherwise → evict to fit, then publish `Ready`.
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
        // Cloned out before anything below can displace this slot. The insertion at the end of
        // this function overwrites `Building` in place rather than going through `Slots::remove`,
        // so it is the one exit from `Building` rule 5's removal choke point does not see, and the
        // handle has to be taken while the variant is still there to take it from.
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

        // Evict BEFORE inserting, so the candidate cannot evict itself: rule 1 keeps `Building` out
        // of the recency index, so it is not a candidate victim, and the loop targets a bound that
        // already leaves room for it.
        //
        // **`pop_first`, not `values().next()` — this is what makes the loop terminate.** Taking the
        // front entry *out* of the index here means every iteration shrinks `recency` by exactly
        // one, whatever the map says about that key, so the loop is bounded by `recency.len()`
        // unconditionally. Selecting without removing, and relying on `Slots::remove` to unlink,
        // makes termination depend on the bijection holding — and `Slots::check` is
        // `debug_assertions`-only, so in a `--release` build a broken bijection is a spin under the
        // request-path mutex rather than an assertion. See rule 1 in this module's doc.
        let mut tally = EvictionTally::default();
        while slots.bytes + charged > bound {
            let Some((_, victim)) = slots.recency.pop_first() else {
                // Nothing left to evict: every remaining slot is `Building`. The candidate is
                // admitted anyway rather than refused — rule 3 — and the transient overshoot is
                // bounded by the in-flight builds, which hold their memory regardless of this map.
                break;
            };
            let (young, freed) = match slots.map.get(&*victim) {
                Some(Slot::Ready { uses, charged, .. }) => (*uses == 0, *charged),
                _ => (false, 0),
            };
            // Counted only when a value actually came back. The `None` arm means the index named a
            // key the map does not hold as `Ready` — a broken bijection — and counting it would put
            // the operator's `evictions`/`evicted_bytes` gauges permanently ahead of the bytes
            // actually freed, i.e. exactly the reading that would say the bound is being enforced
            // while it is not.
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
        // Rule 5's insertion half, and the only exit from `Building` that hands waiters a value.
        // After the insert, so a waiter re-reading the map on wake sees `Ready` rather than the
        // slot it went to sleep on.
        wake.notify_all();
        tally
    }

    /// Remove every entry whose key `keep` rejects; returns how many were removed.
    ///
    /// **The predicate sees keys only, and `Building` slots are removed too.** Both halves are
    /// load-bearing. A predicate over *values* could not see a `Building` slot at all, so a prune
    /// would skip it, and the publish that followed would find its own slot intact, pass the
    /// sequence check and publish an entry under a key the prune had just declared unreachable —
    /// the whole failure rule 2 exists to prevent, reintroduced through the predicate's signature.
    /// Removing the `Building` slot is what makes the sequence check fire.
    ///
    /// **This is an O(n) pass under the request-path lock.** [`CacheStats::prune_scanned`] makes
    /// its cost observable; see `RowProjectionCache::prune_token` for the arithmetic and the threat
    /// model.
    ///
    /// **`keep` runs with the slot lock held**, unlike [`Self::get_or_build`]'s `build`, which goes
    /// out of its way to run with no lock held at all. Stated because the asymmetry is the exact
    /// shape a reader will get wrong: a predicate that consults the engine — or anything that could
    /// re-enter this cache — self-deadlocks on a non-reentrant `Mutex`. Keep it a pure function of
    /// the key, which is all either pruner needs. There is no cheap way to relax this: releasing
    /// the lock to evaluate the predicate would mean re-checking every key on re-acquisition, and
    /// the predicate is a couple of integer comparisons, so the constraint costs nothing to honour.
    pub(crate) fn retain_keys(&self, keep: impl Fn(&K) -> bool) -> usize {
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

    /// Read `key` **without claiming its slot**, distinguishing the three states.
    ///
    /// [`Self::get_or_derive`] cannot serve this purpose: a miss there inserts `Building`, which
    /// commits the caller to producing a value and 429s every other arrival until it does.
    /// Decision 0044's stale-serve path needs the opposite — a caller that finds a miss and then
    /// declines to build, because a background refresh is producing the same key.
    ///
    /// **A hit touches recency, exactly as a hit through `get_or_derive` does.** A serve is a use;
    /// counting it as one is what keeps a session that is being served from stale entries from
    /// having its live entry evicted underneath it as "cold".
    pub(crate) fn peek(&self, key: &K) -> Peek<V> {
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

    /// Every `Ready` entry as `(key, value)`, **most recently used first** — the background
    /// refresh's input.
    ///
    /// **The order is the refresh's, and it is load-bearing.** `refresh_resident` is a serial loop
    /// and a full rebuild is a *measured* 1 277 ms at 10⁹, so across a compaction the pass runs for
    /// minutes and every key it has not reached is shed 429 (compaction §6.2). In map order the
    /// session that waits longest is arbitrary; in this order the tail lands on the sessions that
    /// asked least recently, which are the ones least likely to ask during it. It does not shorten
    /// the window — nothing but doing less work does — it decides who pays for it.
    ///
    /// Taken from [`Slots::recency`] rather than by sorting `map`: that index already holds exactly
    /// the `Ready` slots in oldest-use order (rule 1), so this is a reversed walk of it rather than
    /// an O(n log n) pass under the request-path lock.
    ///
    /// **Not a recency touch.** The refresh is not a use: counting it as one would keep an entry
    /// whose session has gone away young for ever, since the refresh would touch it at every
    /// publication and the LRU would never reach it. The entry the refresh *produces* starts at
    /// the current tick like any other insert, so a session that stops asking still ages out.
    pub(crate) fn ready_entries(&self) -> Vec<(K, Arc<V>)>
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
                // direction — a key the refresh does not see rebuilds on its session's next
                // request.
                _ => None,
            })
            .collect()
    }

    /// **The only place this module *takes* the lock**, and the reason [`CacheStats::slot_locks`]
    /// is a statement about the type rather than about one call path. Any new acquisition added
    /// here has to come through this function, or the exact-count tests stop covering the property.
    /// [`Self::is_locked_now`]'s `try_lock` is the one deliberate exception and both docs say so.
    ///
    /// A poisoned mutex is recovered from rather than propagated, on the same reasoning
    /// `crate::pins::PinManager::lock_drain` gives: nothing under this lock is a partially-applied
    /// invariant a panic could leave torn — [`Slots::remove`] is the only multi-structure mutation
    /// and it cannot panic between its three updates — so refusing every subsequent request because
    /// one unrelated thread unwound would fail closed on availability while buying no safety.
    fn lock_slots(&self) -> MutexGuard<'_, Slots<K, V>> {
        self.slot_locks.fetch_add(1, Ordering::Relaxed);
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the slot lock is held *at this instant* — the probe that makes rule 4 (evicted
    /// `Arc`s drop outside the lock) an assertion rather than a comment.
    ///
    /// **Three caveats, stated because a probe whose limits are unstated is worse than none.** It
    /// is a `try_lock`, so it cannot distinguish this thread holding the lock from another thread
    /// holding it — only a single-threaded test may read it as "the caller holds it". A *poisoned*
    /// mutex also reports `true`, so a test using it must not run after an unrelated panic. And it
    /// is **not** counted in [`CacheStats::slot_locks`], deliberately: counting it would perturb
    /// the exact-count assertions it is used alongside.
    ///
    /// `cfg(test)` so it does not exist in a shipped build at all.
    #[cfg(test)]
    pub(crate) fn is_locked_now(&self) -> bool {
        self.slots.try_lock().is_err()
    }
}

/// Removes *this build's* `Building` slot if the build unwinds. Armed for the whole build,
/// disarmed once the publish path has decided the slot's fate.
///
/// **Compares `seq`, not just state.** A prune landing mid-build removes this slot; a later caller
/// then misses and inserts its own `Building` under the same key. A guard removing by key alone
/// would delete that later builder's slot, wedging *it* at a miss it never sees resolved. No such
/// interleaving exists today, because nothing removes a slot between the insert and the guard —
/// [`SingleFlightCache::retain_keys`] is what makes it reachable, which is why the guard is
/// hardened in the change that introduces pruning rather than after it.
struct RemoveOnUnwind<'a, K: Eq + Hash + Clone, V: CacheWeight> {
    cache: &'a SingleFlightCache<K, V>,
    key: Arc<K>,
    seq: u64,
    disarmed: bool,
}

impl<K: Eq + Hash + Clone, V: CacheWeight> Drop for RemoveOnUnwind<'_, K, V> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // This guard's own `drop` runs while a panic is already unwinding through it, so a poisoned
        // mutex must not be treated as a second panic here — that would abort the process instead
        // of completing the unwind. `lock_slots` recovers rather than propagating, and its doc
        // discharges why that is sound for this guarded value.
        let mut slots = self.cache.lock_slots();
        if matches!(slots.map.get(&*self.key), Some(Slot::Building { seq, .. }) if *seq == self.seq)
        {
            // A `Building` slot carries no value and no charged bytes, so this removal frees
            // nothing that could convoy — rule 4 is satisfied vacuously here rather than by
            // deferral. If this guard ever becomes able to remove a `Ready` slot, that stops being
            // true and the value must be carried out of the critical section like every other path;
            // the binding and the assertion are what make that change loud rather than silent.
            let no_value = slots.remove(&self.key);
            debug_assert!(no_value.is_none(), "a Building slot carries no value");
            self.cache.entries.store(slots.map.len(), Ordering::Relaxed);
            slots.check();
        }
    }
}

/// The floor must never become an *under*-charge as an entry's own bookkeeping grows. This checks
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
    /// The two-argument form the cases below are written in: no source key, so `make`'s `Option`
    /// is always `None`. Production has exactly one call site and it always offers a source.
    trait GetOrBuild<K, V> {
        fn get_or_build(&self, key: K, build: impl FnOnce() -> V) -> Result<Arc<V>, Building>;
        fn get_or_build_waiting(
            &self,
            key: K,
            cancel: &CancelToken,
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
            cancel: &CancelToken,
            build: impl FnOnce() -> V,
        ) -> Result<Arc<V>, WaitEnded> {
            self.get_or_derive_waiting(key, None, cancel, |source| {
                assert!(source.is_none(), "no source key was offered");
                build()
            })
        }
    }

    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A generous bound on the builder handshakes below: long enough that no legitimate run ever
    /// approaches it, short enough that a regression reintroducing blocking (the exact bug this
    /// module's single-flight design exists to prevent) fails the test with a clear panic message
    /// instead of hanging the test binary until a CI timeout kills it.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

    /// A test value of a chosen weight. `u32` cannot carry one, and every eviction assertion below
    /// needs entries whose sizes it controls.
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

    /// D-G's core claim, reproduced deterministically (no sleeps, no timing slack): a concurrent
    /// arrival on the same key while a build is in flight gets `Building` immediately rather than
    /// blocking, and once the build publishes `Ready`, both the retried loser and a fresh arrival
    /// observe the built value without rebuilding.
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

    // ---- Rule 5: waiting (decision 0058) ----------------------------------------------------

    /// Block until some caller is parked, or fail the test.
    ///
    /// **Polls the gauge rather than sleeping a guessed interval.** `waiters_now` is incremented
    /// before the caller sleeps and decremented on every wake, so a test that observes `1` knows a
    /// caller reached the wait — which is the interleaving these cases need and the thing a
    /// `sleep(50ms)` would only assume. It dips to `0` between ticks, so this waits for the first
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

    /// **The property the whole change exists to create** (decision 0058): the racer is served the
    /// winner's value, and the expensive build ran exactly once.
    ///
    /// The build count is the half that a wait implemented as "sleep, then rebuild" would fail
    /// while still returning the right value.
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
            waiter_cache.get_or_build_waiting(1, &CancelToken::new(), move || {
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
            "a served wait is not a refusal — 0058's counter split"
        );
    }

    /// Wake path 3: the builder unwinds, [`RemoveOnUnwind`] removes the slot, and the waiter is
    /// woken. **It must not inherit the panic and must not hang** — it sees what a fresh arrival
    /// would see, a miss, and builds. That the rebuild can panic again is correct and is I13a: no
    /// failure is cached, so every caller finds out for itself.
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
            waiter_cache.get_or_build_waiting(1, &CancelToken::new(), || Weighed(42, BIG))
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

    /// Wake path 2: the value exceeds the whole bound, so the publish removes the `Building` slot
    /// and publishes nothing. The waiter must see a miss and build — the arm's own comment warns
    /// that leaving the slot is "a permanent `ProjectionBuilding` wedge", and with waiters that
    /// hazard becomes a permanent stall, so this is the test that says it did not reappear.
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
            waiter_cache.get_or_build_waiting(1, &CancelToken::new(), || Weighed(42, BIG))
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

    /// Wake path 4: a prune removes the `Building` slot mid-build. The waiter is woken, finds its
    /// key absent, and builds — and the original builder's publish is then a no-op because rule
    /// 2's `seq` check sees the waiter's slot, not its own. **Both halves matter**: without the
    /// wake the waiter stalls to the budget, and without the `seq` check the prune is silently
    /// undone on top of the waiter's entry.
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
            waiter_cache.get_or_build_waiting(1, &CancelToken::new(), || Weighed(42, BIG))
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
    /// than a hang. A build that never finishes yields the same `Building` the immediate refusal
    /// used to, and it is counted in the same place.
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
        let waited = cache.get_or_build_waiting(1, &CancelToken::new(), || {
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

    /// A disconnected client releases its compute permit within a tick instead of holding it to
    /// the budget — the second of decision 0058's two conditions. The budget here is two orders of
    /// magnitude longer than the assertion, so a waiter that ignored the token would fail the
    /// elapsed check rather than merely being slow.
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

        let cancel = CancelToken::new();
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
            "a client's own disconnect is not backpressure and must not be counted as one"
        );

        release_tx.send(()).unwrap();
        builder.join().unwrap().unwrap();
    }

    /// A waiter woken by a removal may find that *another* caller has since claimed the key, and
    /// it then waits for that one instead of refusing — the budget, not the number of rounds, is
    /// what bounds the loop.
    ///
    /// **This is where a `seq`-carrying waiter would differ, and why this module does not have
    /// one.** The waiter is served the second builder's value. That is correct rather than
    /// tolerated: the key wholly determines the value here, so the second build's answer is the
    /// first's, and refusing it would rebuild a value already in the map.
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
                waiter_cache.get_or_build_waiting(1, &CancelToken::new(), || Weighed(42, BIG))
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

    /// The map lock is held only for the O(1) transition, never for the build — proven by having
    /// key `1`'s build block indefinitely while key `2`'s build runs and completes on another
    /// thread. If the lock were held across the build (the anti-fix the F4 memo names — merely
    /// narrowing the critical section without moving the build outside it), key `2` would hang.
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

    /// I13a: a panicking build must never leave a permanent `Building` wedge. The entry is absent
    /// afterwards (not `Building`, not a cached failure), so the very next call retries cleanly.
    #[test]
    fn a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds() {
        let cache = unbounded();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.get_or_build(7, || -> Weighed { panic!("boom") })
        }));
        assert!(result.is_err(), "the panic must propagate to the caller");
        assert_eq!(
            cache.len(),
            0,
            "a panicked build must not leave a Building wedge (I13a)"
        );

        let rebuilt = cache.get_or_build(7, || Weighed(7, BIG)).unwrap();
        assert_eq!(rebuilt.0, 7);
        assert_eq!(cache.len(), 1);
    }

    /// **The counted choke point, hit path.** A warm hit takes exactly one acquisition: the LRU
    /// touch happens inside it and adds none of its own. A recency index behind its own mutex, or a
    /// re-lock to update it, shows up here and nowhere else — no criterion bench on this box can
    /// see 15–25 ns against a millisecond request.
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

    /// **The counted choke point, miss path.** Exactly two per miss: publish `Building`, then
    /// publish `Ready`. The eviction pass runs inside the second. A third acquisition to evict
    /// after publishing — the natural implementation — fails here.
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

    /// **Rule 4, made assertable.** A value whose `Drop` observes the lock state records a
    /// violation if it is dropped inside the critical section. Deterministic, single-threaded, no
    /// timing: move the `dead` vector's drop inside the locked block and this fails every run.
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
        // Forces an eviction. The evicted value is uniquely held by the cache — the `Arc`s returned
        // above were dropped at the end of each statement — so it really does drop on this call.
        cache.get_or_build(3, || Tattle(BIG)).unwrap();

        PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(cache.stats().evictions >= 1, "the test must have evicted");
        assert!(
            !VIOLATION.with(Cell::get),
            "an evicted value was dropped while the slot lock was held (rule 4): dropping a \
             ~125 MB bitmap frees ~15k containers and convoys every admitted request"
        );
    }

    /// **Rule 4 on the pruning path** — the half `evicted_arcs_are_dropped_outside_the_lock` does
    /// not reach, and the one that matters more.
    ///
    /// The eviction pass frees one entry at a time on a *miss*, which is already a multi-second
    /// build. A prune frees a whole session's entries at once, on `/session/revoke`, which is
    /// deliberately **outside** the admission gate (D13) — so `dead.clear()` inside `retain_keys`'s
    /// critical section is n × ~125 MB of bitmaps, ~15 k containers each, freed while all 48
    /// admitted requests block on the same mutex. That mutation left the whole workspace green
    /// before this test existed (round-1 review, MX2).
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
            "a pruned value was dropped while the slot lock was held (rule 4): /session/revoke is \
             outside the admission gate, so this convoys every admitted request"
        );
    }

    /// **Rule 1.** A `Building` slot is never chosen as an eviction victim: it holds no value to
    /// free, and evicting it would let a second caller start a duplicate multi-second build.
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

    /// **The I13a wedge the oversized path would otherwise leave.** A value larger than the whole
    /// bound is served to its caller (rule 3) and not retained — and its `Building` slot is
    /// *removed*, so the next arrival sees a plain miss rather than a permanent
    /// `ProjectionBuilding` refusal for the rest of that session.
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
            "the Building slot must be REMOVED, not left behind: a Building slot with no builder \
             is a permanent ProjectionBuilding wedge for that key (I13a)"
        );

        // The wedge test proper: the very next arrival must be able to build, not be refused.
        let again = cache.get_or_build(1, || Weighed(6, BIG * 4));
        assert!(
            matches!(&again, Ok(v) if v.0 == 6),
            "a second arrival on an oversized key must rebuild, not get Building forever"
        );
    }

    /// The byte bound holds, and the victim is the least recently *used*, not the least recently
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

    /// `young_evictions` counts only entries that were never reused — the thrash signature. An
    /// entry hit before being evicted is a healthy cold eviction and must not alarm.
    #[test]
    fn young_evictions_counts_only_never_reused_entries() {
        let cache = SingleFlightCache::<u32, Weighed>::new(BIG * 2);
        cache.get_or_build(1, || Weighed(1, BIG)).unwrap();
        cache.get_or_build(2, || Weighed(2, BIG)).unwrap(); // key 2 is never reused
                                                            // The touch must come AFTER key 2 is published, or key 1 is still the older entry in
                                                            // recency order and it, not key 2, is the first victim — which is what this test asserted
                                                            // on its first run, and why the ordering here is load-bearing rather than incidental.
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

    /// **Rule 2.** A prune landing mid-build must not be undone by the publish that follows it. The
    /// build completes and its caller receives the value (rule 3), but nothing is republished under
    /// the pruned key.
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

    /// **Rule 2, the sharper half.** A build that unwinds after its slot was pruned and a *later*
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

    /// **Forward progress under a bound below the working set.** Five keys round-robin through a
    /// cache that holds two: every call still returns its own value, and no call is ever refused
    /// with `Building` — refusals come from *same-key* concurrency, which a round-robin never
    /// creates. The hit rate collapsing is expected and is what the startup validation exists to
    /// prevent; the failure this guards is a bound that bites turning into a refusal.
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

    /// **`ready_entries` is most-recently-used first, and reading it is not a use.**
    ///
    /// The refresh is a serial loop over a rebuild that costs a *measured* 1 277 ms at 10⁹, so
    /// across a compaction it runs for minutes and every key it has not reached is shed 429
    /// (compaction §6.2). Ordering does not shorten that window; it decides who waits in it, and
    /// the answer must be the sessions that asked least recently. In `map` order — which is what
    /// this replaced — the tail was whatever `FxHashMap` happened to yield.
    ///
    /// **Mutations this kills:** iterating `map` instead of `recency` (leg 1 fails on order, or
    /// flakes, which is itself the point); dropping the `.rev()` (leg 1 reverses); touching
    /// recency inside `ready_entries` (leg 2 sees the order change under a read).
    #[test]
    fn ready_entries_are_most_recently_used_first_and_reading_them_is_not_a_use() {
        let cache = unbounded();
        for key in 0..4u32 {
            cache.get_or_build(key, || Weighed(key, BIG)).unwrap();
        }
        // Re-touch 1, so use order is 2, 3, 0, 1 oldest-first.
        cache.get_or_build(0, || panic!("warm")).unwrap();
        cache.get_or_build(1, || panic!("warm")).unwrap();

        let order: Vec<u32> = cache.ready_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            order,
            vec![1, 0, 3, 2],
            "the refresh must reach the most recently used session first"
        );

        // Leg 2: a second read sees the same order. A recency touch here would make the entry the
        // refresh visits at every publication immortal, since the LRU would never reach it.
        let again: Vec<u32> = cache.ready_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(again, order, "reading the list must not reorder it");
    }
}
