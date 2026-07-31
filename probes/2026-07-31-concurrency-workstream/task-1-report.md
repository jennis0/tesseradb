# Task 1 report — F4: slot-state single-flight for the row-projection cache (D-G)

## Summary

Replaced `Engine::row_projection_cache`'s `Mutex<FxHashMap<Key, Arc<RowProjection>>>` (which ran
the expensive `RowProjection::new` build *inside* the map lock on a miss) with a slot-state
single-flight cache per design decision D-G: `Building | Ready(Arc<RowProjection>)`, map lock held
only for the O(1) state transition, build always runs with the lock released. A concurrent arrival
observing `Building` does not wait — it gets the new `EngineError::ProjectionBuilding` and is
expected to retry (D-G's non-blocking-waiters rule: a parked waiter would hold the server's future
admission budget while burning zero CPU).

The state machine is implemented as a new, engine-independent generic type,
`crate::single_flight::SingleFlightCache<K, V>` (`crates/tessera-engine/src/single_flight.rs`),
rather than inline in `session.rs`/`viewport.rs`. This is not a scope expansion — it is what let the
core concurrency properties (single-flight, non-blocking waiters, panic safety) be proven
**deterministically**, with channels controlling exactly when a build starts and finishes, instead
of only via real-thread timing races. `Engine`'s `row_projection_cache` field is now a
`SingleFlightCache<(u64, String, u64), RowProjection>`; `Engine::viewport`'s cache lookup in
`viewport.rs` calls `get_or_build` with a closure that calls `probe.mark_projection_built()` (moved
inside the builder path, per the brief) and then `RowProjection::new`.

## Files changed

- `crates/tessera-engine/src/single_flight.rs` (new) — `SingleFlightCache<K, V>`, the D-G slot-state
  map, plus 4 deterministic unit tests.
- `crates/tessera-engine/src/lib.rs` — registers the new private module.
- `crates/tessera-engine/src/session.rs` — `row_projection_cache` field retyped to
  `SingleFlightCache<...>`; `EngineError::ProjectionBuilding` variant added (`Debug`/`Display`);
  `row_projection_cache_len` doc updated (counts `Building` + `Ready` slots).
- `crates/tessera-engine/src/viewport.rs` — the inline lock/check/build/insert block replaced with
  `self.row_projection_cache.get_or_build(cache_key, || { probe.mark_projection_built(); RowProjection::new(...) }).map_err(|_| EngineError::ProjectionBuilding)?`,
  with a comment citing the F4 memo and warning against narrowing the critical section instead of
  moving the build outside it.
- `crates/tessera-engine/tests/viewport.rs` — fixture writers parameterised over item count
  (`write_points_n`/`write_pairs_n`/`build_fixture_n`, with `build_fixture` now a thin
  `N_ITEMS`-sized wrapper — no existing call site changed); three new integration tests (see below).
- `crates/tessera-server/src/error.rs` — `map_engine_error` gets an explicit
  `EngineError::ProjectionBuilding => ApiError::FailClosed(...)` arm (was previously going to be
  caught only by the `other =>` wildcard; the brief asked for it to be explicit and visible at the
  call site) plus one test.

## TDD evidence

### RED

Backed up the four source files, reverted them to the pre-fix state (`Mutex<FxHashMap<Key,
Arc<RowProjection>>>`, no `single_flight` module, no `ProjectionBuilding` variant), kept only the
test-file changes, and ran:

```
$ cargo test -p tessera-engine --test viewport -- concurrent_same_key_viewports_single_flight_others_get_projection_building distinct_key_first_viewports_overlap_instead_of_serialising warm_row_projection_cache_serves_output_identical_to_cold
```

```
error[E0599]: no variant, associated function, or constant named `ProjectionBuilding` found for enum `EngineError` in the current scope
    --> crates/tessera-engine/tests/viewport.rs:1876:54
     |
1876 |             .filter(|r| matches!(r, Err(EngineError::ProjectionBuilding)))
     |                                                      ^^^^^^^^^^^^^^^^^^ variant, associated function, or constant not found in `EngineError`
error[E0599]: no variant, associated function, or constant named `ProjectionBuilding` found for enum `EngineError` in the current scope
    --> crates/tessera-engine/tests/viewport.rs:1900:50
error: could not compile `tessera-engine` (test "viewport") due to 2 previous errors
```

(The `single_flight` unit tests are new against a wholly new module, so their RED state is
"the module does not exist yet" — trivially confirmed the same way, by their absence before the
module was added.)

Restored the four source files from the backup afterwards (`git stash` used to isolate the revert;
diff confirmed byte-identical to the pre-revert working tree before dropping the stash).

### GREEN

```
$ cargo test -p tessera-engine --test viewport
running 32 tests
... (31 ok, 1 ignored pre-existing #[ignore] real-corpus test) ...
test result: ok. 31 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.32s

$ cargo test -p tessera-engine --lib single_flight
running 4 tests
test single_flight::tests::a_panicking_build_leaves_the_key_absent_so_a_retry_rebuilds ... ok
test single_flight::tests::a_ready_hit_never_calls_build_again ... ok
test single_flight::tests::concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild ... ok
test single_flight::tests::distinct_keys_never_contend_on_a_slow_build ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out; finished in 0.00s

$ cargo test -p tessera-server --lib error
test error::tests::map_engine_error_takes_projection_building_to_the_fail_closed_arm ... ok
... (3 passed, 0 failed)
```

Both new concurrency-flavoured integration tests and the four `single_flight` unit tests were run
5x and 3x respectively back-to-back with no failures.

## Test design notes (why two tiers)

The brief's verify list (exactly-one-build, others get `ProjectionBuilding`, eventual success,
distinct-key overlap, panic-safety) is split across two tiers rather than forced entirely into
black-box `Engine::viewport` tests:

1. **Deterministic unit tests** (`crates/tessera-engine/src/single_flight.rs`, `#[cfg(test)] mod
   tests`) against `SingleFlightCache<K, V>` directly — no bundle, no `Engine`, no timing. Two
   `mpsc` channels give full control over exactly when a build starts and finishes, so
   "a concurrent miss during a build gets `Building` without blocking, and the loser's own closure
   never runs" is proven by construction, not by hoping a race lands. This is also where panic
   safety is proven (`std::panic::catch_unwind`, entry absent afterwards, clean retry) and where
   "distinct keys never contend on a slow build" is proven (key 2's build completes while key 1's
   is still blocked mid-build) — directly exercising the brief's "map mutex held only for O(1)
   transitions" claim rather than inferring it from timing.

   This matches the brief's explicit allowance ("it is acceptable to unit-test the drop-guard/
   slot-removal behaviour in a `#[cfg(test)]` module inside the engine crate where the map is
   directly reachable") — extended from panic-safety alone to the whole state machine, because
   making the single-flight logic a small standalone generic type (rather than inlined into
   `Engine`) made the whole thing reachable that way, with no test-only API added to `Engine`
   itself.

2. **Integration tests through the public `Engine::viewport` API**
   (`crates/tessera-engine/tests/viewport.rs`), demonstrating the fix end to end:
   - `concurrent_same_key_viewports_single_flight_others_get_projection_building` — up to 25 rounds
     of 16 real OS threads (`std::sync::Barrier`-synchronised) racing the *same* freshly-authorised
     session's first viewport over a 150k-item synthetic fixture (chosen so a cold build takes tens
     of ms even unoptimised — comfortably above OS thread-wake jitter); a round with no observed
     contention is not a failure (an unlucky schedule), only exhausting every round without ever
     observing `ProjectionBuilding` is. Once contention lands: asserts every result is success or
     `ProjectionBuilding` (never anything else), retries every loser and confirms it now succeeds,
     and confirms the cache holds exactly one slot per round's key.
   - `distinct_key_first_viewports_overlap_instead_of_serialising` — times N=4 fresh sessions' cold
     first viewports run serially vs. released together on N threads; asserts `concurrent < 0.7 *
     serial` (skips on a single-core box). Measured on this 12-core dev machine: `serial=282.6ms
     concurrent=79.0ms`.
   - `warm_row_projection_cache_serves_output_identical_to_cold` — the byte-format-unchanged
     constraint: cold and warm calls produce `PartialEq`-equal `ViewportOut` (the hand-written impl
     already excludes only `timings`), and the cache never grows past one slot for one session.

   The module doc comment at the top of this section in `tests/viewport.rs` states this division
   explicitly, so a future reader isn't left wondering why the black-box tests don't also assert
   "exactly one build" by counting.

`write_points`/`write_pairs` (the old unparameterised writers) were deleted rather than kept as
unused wrappers once `build_fixture` was rewritten to call `build_fixture_n` directly — clippy
would otherwise have flagged them dead code.

## Server-side mapping

`map_engine_error` (`crates/tessera-server/src/error.rs`) already had a `other => FailClosed` catch-
all, so the match would have compiled either way; the brief asked for the new variant to be named
explicitly rather than silently inherited from the wildcard, which is what the added arm does:

```rust
building @ EngineError::ProjectionBuilding => ApiError::FailClosed(building.to_string()),
```

with a comment stating this is transitional (Task 4 maps it to 429 + `Retry-After`) and a test
(`map_engine_error_takes_projection_building_to_the_fail_closed_arm`) pinning the current 500.

## Verification run

- `cargo test -p tessera-engine --test viewport` — 31 passed, 1 ignored (pre-existing), 0 failed.
- `cargo test -p tessera-engine --lib` — 12 passed (includes the 4 new `single_flight` tests).
- `cargo test -p tessera-server --lib error` / `--test http` — all green (26 http tests unaffected).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo clippy -p tessera-engine --all-targets --features bench-timing -- -D warnings` — clean
  (checked separately since `bench-timing` changes the `Probe`/closure interaction at the cache
  call site).
- `cargo fmt --check -p tessera-engine -p tessera-server` — clean.
- `cargo test --workspace` — green **except** `tessera-store`'s pre-existing
  `write_permutation_rejects_entity_id_not_fitting_u32`, which SIGKILLs (OOM) on this machine even
  run alone (`cargo test -p tessera-store --test segment_roundtrip write_permutation_rejects_entity_id_not_fitting_u32`
  reproduces it in isolation). This test allocates near-`u32::MAX`-scale structures; it passed once
  earlier in this session (66.4s, evidently near the memory ceiling) and failed on a later run under
  more memory/swap pressure (this box: 47 GiB RAM, 12 GiB swap, 7.4 GiB swap already in use at the
  time of the failing run). It is in `tessera-store`, a crate this task never touched, and unrelated
  to the row-projection cache. `cargo test --workspace --exclude tessera-store` and
  `cargo test -p tessera-store -- --skip write_permutation_rejects_entity_id_not_fitting_u32` are
  both fully green. Flagging this as a pre-existing environmental flake outside this task's scope,
  not a regression introduced here.

## Self-review

- **D-G fidelity.** Build runs strictly outside the map lock (`SingleFlightCache::get_or_build`
  drops the lock before calling `build`); a losing concurrent arrival never blocks (returns
  `Err(Building)` synchronously under the same lock acquisition that observed the `Building` state
  — no second lock, no wait); panic safety uses a drop guard armed for the whole build and disarmed
  only after `Ready` is published, so an unwinding build always leaves the key absent, never wedged
  at `Building` and never left `Ready` with a bogus value.
- **Anti-fix check.** Confirmed the build genuinely runs with the lock released (not merely a
  narrower critical section) — this is what the `distinct_keys_never_contend_on_a_slow_build` unit
  test exists to catch mechanically (key 2's build would hang if the lock were held across key 1's
  build), and it passes.
- **I13.** No failure or panic is ever cached; the drop guard removes the entry outright, so the
  very next call sees a plain miss (proven both in the deterministic unit test and structurally in
  the code — the guard's `Drop` is the only removal path, and it is unconditional except for the
  `disarmed` flag set only after a successful `Ready` publish).
- **Byte-format/wire unchanged.** `warm_row_projection_cache_serves_output_identical_to_cold` pins
  this; no field of `ViewportOut` other than `timings` (already excluded from `PartialEq`) can
  differ between cold and warm.
- **Engine API stays sync, no executor, no tokio.** `SingleFlightCache` and `Engine::viewport` are
  both plain synchronous code; no new dependency was added to `tessera-engine`'s `Cargo.toml`.
- **British spelling.** Checked new comments/docs for `authorise`/`authorisation`-style spelling;
  none of the new text needed a US/UK spelling call other than words already spelled correctly
  (no "authorize", "serialize", etc. introduced).
- **`row_projection_cache_len` semantics changed slightly**: it now counts `Building` slots too, not
  only `Ready` ones. This only matters mid-build, which no existing test observes (the C-5 test
  asserting it stays `0` across `Engine::item` calls is unaffected, since `Engine::item` never
  touches the cache at all). Documented at the method's doc comment.
- **Unbounded map growth**: explicitly out of scope per the brief — noted in the field's doc comment
  and in the commit message; no eviction was added.

## Concerns / follow-ups for later tasks

1. **Task 4** still needs to map `EngineError::ProjectionBuilding` to HTTP 429 + `Retry-After` at
   the server boundary; today it is an honest (fail-closed) 500, which is correct but not yet
   actionable for a well-behaved retrying client.
2. **Eviction** of the row-projection cache (`SingleFlightCache` has no capacity bound or TTL) is
   unaddressed, as scoped out by the brief — a memory concern for a future task, not a concurrency
   one.
3. The pre-existing `tessera-store` OOM flake noted above is unrelated to this task but worth a
   maintainer's attention if it recurs in CI (possibly needs `--test-threads=1` for that binary, or
   a smaller fixture in that specific test).
