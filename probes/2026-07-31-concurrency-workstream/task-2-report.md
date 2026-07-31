# Task 2 report — fragment cache slot-state single-flight + in-memory cache

## Starting state

On picking up this task, the worktree already contained a complete, uncommitted implementation
matching the brief exactly (`git status` showed the five files below modified/added, with no SDD
ledger entry yet and no `task-2-report.md`) — evidently produced by an earlier, uncommitted pass
at this same task. Rather than discard and redo, I read every changed file end to end, reconstructed
the RED step independently (see below), re-verified GREEN, ran the full verification battery the
brief requires, self-reviewed the diff line by line, and committed. I did not write any new
production or test code beyond what was already present, because on inspection it correctly
implements D-G and nothing needed correcting.

## Implementation

**`crates/tessera-authz/src/single_flight.rs` (new).** `SingleFlightCache<K, V>` — the fallible
twin of Task 1's `tessera_engine::single_flight::SingleFlightCache`. `Slot<V> = Building |
Ready(Arc<V>)`, one `Mutex<FxHashMap<K, Slot<V>>>`. `get_or_try_build(key, impl FnOnce() ->
Result<V, E>) -> Result<Arc<V>, SingleFlightError<E>>`:
- Hit on `Ready`: clone the `Arc`, return — no `build` call (the in-memory-cache half of D-G).
- Hit on `Building`: `Err(SingleFlightError::Building)` immediately, no waiting.
- Miss: insert `Building`, drop the lock, run `build()` outside it, re-lock, publish `Ready`.
- Fail-closed: a `RemoveUnlessReady` drop guard removes the entry on any early exit — `Err` return
  or unwind — so a failed build never wedges at `Building` and never caches an `Err` (I13). One
  guard covers both failure exits (`Err` and panic), unlike Task 1's infallible cache which only
  needed a panic guard.

**`crates/tessera-authz/src/fragment.rs`.** `FragmentCache` gained a `slots:
SingleFlightCache<[u8; 32], FrozenFragment>` field keyed by the **canonical key** (computed before
the single-flight call, never by `auth_data_hash` — the doc explains why the fast-path hash would
be an I2 hazard as a single-flight key: two different credentials satisfying the same term set
must correctly collapse onto the same build, and a hash collision must never be mistaken for a
build-in-flight signal on the wrong key). `get_or_build`'s body is now
`self.slots.get_or_try_build(key, || { try disk open; else create_private_dir_all + build_fragment
+ rebuild_count++ + build_and_persist })`, mapping `SingleFlightError` to the new
`FragmentCacheError { Building, Io(io::Error) }`. `slot_count()` added (mirrors
`Engine::row_projection_cache_len` from Task 1) for fail-closed wedge assertions. `rebuild_count`
doc updated: increments once per single-flight build, never on a losing arrival's retry-into-`Ready`
hit (lifecycle §3.3).

**`crates/tessera-engine/src/session.rs`.** `EngineError::FragmentBuilding` added alongside the
existing `ProjectionBuilding` (Task 1), with the same transitional-500-until-429 doc language.
`Engine::authorise` maps `FragmentCacheError::Building -> EngineError::FragmentBuilding` and
`FragmentCacheError::Io -> EngineError::Io` (previously the whole call was `.map_err(EngineError::
Io)`, which cannot exist any more since `FragmentCacheError` no longer implements `Into<io::Error>`
losslessly — the match makes the split explicit).

**`crates/tessera-server/src/error.rs`.** `map_engine_error` gained an explicit
`building @ EngineError::FragmentBuilding => ApiError::FailClosed(...)` arm (not left to the
wildcard), matching the existing `ProjectionBuilding` arm, plus a test asserting it takes the
fail-closed 500 today.

**`crates/tessera-authz/src/lib.rs`.** Exports `FragmentCacheError` alongside the existing
`FragmentCache`/`FrozenFragment`; `mod single_flight;` added (private — only `SingleFlightCache`/
`SingleFlightError` used internally by `fragment.rs`, `pub(crate)` visibility).

**`crates/tessera-authz/tests/fragment.rs`.** Three new tests (below); no existing test's
assertions needed adjustment — `rebuild_count` already only counted actual builds pre-D-G (there
was no slot map before, but every `get_or_build` call was already a serial cache-check-then-build,
so single-threaded rebuild-count expectations were unaffected by adding single-flight machinery).

## Reuse decision

**Duplicated `SingleFlightCache` into `tessera-authz/src/single_flight.rs` rather than reusing/
generalising `tessera-engine::single_flight`.** Reason, checked directly against
`scripts/check-layers.sh` rather than assumed: `tessera-engine` depends on `tessera-authz`
(`crates/tessera-engine/Cargo.toml`), so the dependency direction is authz below engine — authz
cannot take a dependency on engine to reuse its module without an illegal back-edge the layering
script would catch. The brief's fallback ("or implement the same shape locally in tessera-authz
with a comment cross-referencing") was the only compliant path without moving the type to a new
shared crate (out of scope for this task, and would touch layering itself). The duplicate's module
doc explicitly cross-references `tessera-engine/src/single_flight.rs` (commit d9baada) and states
the layering reason so a future reader isn't left wondering why there are two near-identical
modules. The two also have genuinely different signatures (`get_or_build: FnOnce() -> V` infallible
vs `get_or_try_build: FnOnce() -> Result<V, E>` fallible), so even a shared crate would need the
generalisation this module already carries — confirmed by re-reading Task 1's `single_flight.rs`
before duplicating, not assumed from the brief's description.

`bash scripts/check-layers.sh` passes (see verification below).

## TDD evidence

The implementation pre-existed in the worktree uncommitted; I reconstructed RED independently
rather than trust that a RED step had genuinely occurred, by isolating the test-file diff against
the pre-Task-2 baseline (`d9baada`) and confirming it fails to compile without the implementation:

```
$ git stash                                  # revert all 5 tracked-file changes to d9baada
$ git diff <pre-task2> <post-task2> -- crates/tessera-authz/tests/fragment.rs > /tmp/.../fragment_test.diff
$ git apply /tmp/.../fragment_test.diff      # new tests only, old implementation
$ cargo test -p tessera-authz --quiet
error[E0432]: unresolved import `tessera_authz::FragmentCacheError`
  --> crates/tessera-authz/tests/fragment.rs:15:68
error[E0599]: no method named `slot_count` found for struct `FragmentCache` in the current scope
  --> crates/tessera-authz/tests/fragment.rs:435:15
error: could not compile `tessera-authz` (test "fragment") due to 2 previous errors
```

RED confirmed (compile failure — the new tests reference `FragmentCacheError` and `slot_count`
that don't exist pre-implementation). Restored the full implementation:

```
$ git checkout -- crates/tessera-authz/tests/fragment.rs
$ git stash pop
$ cargo test -p tessera-authz --quiet
running 16 tests
................
test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.23s
running 9 tests
.........
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
running 2 tests
..
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

GREEN confirmed. The three new fragment tests exercising D-G's exact verify list from the brief:

- `concurrent_cold_builds_single_flight_to_one_real_build` — 8 threads race `get_or_build` on the
  same canonical key from a cold cache via a `Barrier`; a losing arrival retries on `Building`
  (bounded-attempt loop, no sleep — a stuck `Building` fails the test instead of hanging);
  asserts all 8 threads end up with the *same* `Arc<FrozenFragment>` and `rebuild_count() == 1`.
- `warm_hit_does_no_file_io_after_backing_files_are_removed` — warms the cache, deletes every file
  under the cache directory, then asserts a second `get_or_build` call still succeeds, returns the
  identical `Arc` (`Arc::ptr_eq`), and `rebuild_count()` stays at 1 (if it touched disk at all it
  would find nothing and either error or silently rebuild — proves the `Ready` hit path is pure
  in-memory).
- `failed_build_leaves_no_wedge_and_retry_after_repair_succeeds` — points the cache directory's
  parent at a plain file (so `create_dir_all` fails `ENOTDIR`), asserts `FragmentCacheError::Io`,
  `slot_count() == 0`, `rebuild_count() == 0`; repairs the filesystem; asserts the retry succeeds
  and `rebuild_count()` becomes 1.

`single_flight.rs`'s own five unit tests (channel/barrier-synchronised, no sleeps) cover the
state-machine primitives directly: warm hit never calls `build`; a concurrent miss during a build
gets `Building` and doesn't block, and both the retried loser and a fresh arrival see the built
value without rebuilding; two distinct keys never contend (lock not held across a slow build);
`Err`-returning and panicking builds both leave the key absent with a working retry.

## Verification run

```
$ cargo test -p tessera-authz --quiet     # 16+9+2 ok
$ cargo test -p tessera-engine --quiet    # 17+17+12+31(+1 ignored) ok
$ cargo test -p tessera-server --quiet    # 11+26 ok
$ bash scripts/check-layers.sh            # exit 0
$ cargo build --workspace --all-targets --quiet   # clean (examples, benches included)
$ cargo test --workspace                  # all green except the known environmental flake below
```

`tessera-store`'s `write_permutation_rejects_entity_id_not_fitting_u32` was SIGKILLed (signal 9)
during the full-workspace run — this is the documented pre-existing OOM flake (progress.md line 2,
task instructions), not caused by this change; every other test binary in the workspace run
(23 `test result: ok` blocks) passed with 0 failures. Not chased, per instructions.

## Files changed

- `crates/tessera-authz/src/single_flight.rs` (new) — fallible `SingleFlightCache`
- `crates/tessera-authz/src/fragment.rs` — slot-state map, `FragmentCacheError`, `slot_count`
- `crates/tessera-authz/src/lib.rs` — export `FragmentCacheError`, add `mod single_flight`
- `crates/tessera-authz/tests/fragment.rs` — 3 new D-G tests
- `crates/tessera-engine/src/session.rs` — `EngineError::FragmentBuilding`, mapping in `authorise`
- `crates/tessera-server/src/error.rs` — explicit `FragmentBuilding` fail-closed arm + test

## Self-review

- Canonical-key-only single-flighting confirmed: `slots.get_or_try_build(key, ...)` is called with
  the canonical `key` variable (post key-memo resolution), never `auth_data_hash` — matches the
  brief's explicit I2 hazard warning (fragment.rs:396-406-era doc, now the `get_or_build` doc
  block).
- Never-cache-an-`Err` confirmed both at the generic `single_flight.rs` level (drop guard covers
  `Err` and panic) and exercised concretely by `failed_build_leaves_no_wedge_and_retry_after_repair_
  succeeds`.
- `rebuild_count` semantics re-read against lifecycle §3.3's intent: "once per single-flight
  build" — verified by the concurrent test (8 racing threads, 1 rebuild) and the warm-hit test
  (second call doesn't bump it).
- Checked every other call site of `FragmentCache::get_or_build`/`::new`/`.rebuild_count()`
  (bench arms, engine tests, an example, a benchmark) via `grep` — all use `?` or `.unwrap()` on
  the result and compile unmodified against the new `Result<_, FragmentCacheError>` return type
  (was `io::Result`); `cargo build --workspace --all-targets` confirms.
- Re-read the `single_flight.rs` module doc's layering claim against `crates/tessera-engine/
  Cargo.toml` directly rather than trusting the prose — confirmed `tessera-engine` depends on
  `tessera-authz`, so the stated direction is correct and duplication (not a shared-crate move) is
  the only compliant fix within this task's scope.
- No `unwrap`/`expect` added on a fallible path outside tests; production error handling is total
  (`Building` and `Io` both handled explicitly at every `?`/`map_err` boundary touched).

## Concerns

- None blocking. One pre-existing, explicitly out-of-scope item persists: `EngineError::
  FragmentBuilding` (like `ProjectionBuilding` from Task 1) maps to a fail-closed HTTP 500 rather
  than 429 today — this is the brief's stated transitional state, to be wired by a later task, and
  both arms are named explicitly (not left to a wildcard) so that later mapping is a one-line
  change per error variant, not a rediscovery.
- The environmental OOM flake on `tessera-store`'s `write_permutation_rejects_entity_id_not_fitting_
  u32` reproduced again during the full-workspace run, as documented — unrelated to this task,
  not investigated further per instructions.
