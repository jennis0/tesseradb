# Fix-wave report — 25 deferred minors, Tasks 1-9

Scope: every `minor (deferred)` line in `progress.md` (25 of them). Out of scope (per instructions):
the `important (deferred, ruling)` unbounded-cache-growth item, `note:` lines, Task 9 acceptance
items, TILE_PAR_MIN_LEN/tile-loop-threshold calibration, and any new features.

**Starting state.** The worktree already contained substantial uncommitted work from an earlier,
interrupted pass at this same fix wave (`git status` showed 9 modified files before this session
touched anything). Every one of those pre-existing diffs was independently re-verified against its
ledger line (read in full, reasoned about, not just trusted) before being counted as done, and is
listed below with `[pre-existing, verified]`.

## Checklist

### Task 1 (single_flight.rs, session.rs, tests/viewport.rs)

1. **FIXED** `[pre-existing, verified]` — warm-hit `key.clone()` only on miss.
   `crates/tessera-engine/src/single_flight.rs:86-94` (`get_or_build`) — lookup by reference first,
   clone only on the miss path. Covering test: `cargo test -p tessera-engine --lib single_flight`
   (`a_ready_hit_never_calls_build_again` and the other 3 unit tests), all green.

2. **FIXED** `[pre-existing, verified]` — single-flight unit tests hang-not-fail — `recv_timeout`.
   `crates/tessera-engine/src/single_flight.rs:150,185-193,233-235` — `HANDSHAKE_TIMEOUT =
   Duration::from_secs(5)` + `.recv_timeout(...)` replacing bare `.recv()` in
   `concurrent_miss_during_a_build_does_not_block_and_does_not_rebuild` and
   `distinct_keys_never_contend_on_a_slow_build`. Covering test: same as above.

3. **FIXED** `[pre-existing, verified]` — overlap timing test core-count gating.
   `crates/tessera-engine/tests/viewport.rs:1945-1953` — skip threshold raised from `cores < 2` to
   `cores < 4` with a doc explaining the headroom reasoning. Covering test:
   `cargo test -p tessera-engine --test viewport distinct_key_first_viewports_overlap_instead_of_serialising`
   (passes; this box has 12 cores so the gate itself isn't exercised, but the logic was inspected).

4. **SKIPPED** — "TDD RED was compile-failure only for two of three tests." This is a retrospective
   honesty note in `task-1-report.md` about the *quality* of RED evidence already gathered (the
   third test, a byte-equality check, has no meaningful behavioural RED). The ledger line names no
   concrete code fix, and there is no code defect to correct — re-litigating historical TDD evidence
   isn't an available action. No change made.

5. **FIXED** `[pre-existing, verified]` — `get_or_build`'s `FnOnce() -> V` can't express a fallible
   build — doc line added. `crates/tessera-engine/src/single_flight.rs:73-78` — "Invariant: `build`
   must be infallible" paragraph, pointing at `tessera-authz`'s `get_or_try_build` twin. Covering
   test: doc-only, verified by `cargo doc`/compile (part of the standard build).

6. **FIXED** `[pre-existing, verified]` — stale `#[allow(clippy::type_complexity)]` on the
   now-simplified `row_projection_cache` field. `crates/tessera-engine/src/session.rs:296` — the
   `#[allow]` (present when the field was `Mutex<FxHashMap<(u64,String,u64), Arc<RowProjection>>>`)
   removed now that the type is the much simpler `SingleFlightCache<(u64,String,u64),
   RowProjection>`. Covering: `cargo clippy -p tessera-engine --all-targets -- -D warnings` clean
   (no `type_complexity` warning fires without the field's own allow, confirming it was genuinely
   unneeded, not silently still-required).

### Task 2 (single_flight.rs twins, tests/fragment.rs)

7. **FIXED** `[pre-existing, verified]` — Drop guard `self.slots.lock().unwrap()` double-panic
   hazard under a poisoned mutex — `unwrap_or_else(PoisonError::into_inner)` in both twins.
   `crates/tessera-engine/src/single_flight.rs:113-117` and
   `crates/tessera-authz/src/single_flight.rs:120-124`, each with a comment explaining why
   recovering a poisoned lock in a `Drop` impl mid-unwind is sound here. Covering tests:
   `cargo test -p tessera-engine --lib single_flight` and `cargo test -p tessera-authz --lib
   single_flight`, both green (the panic-safety tests exercise the guard's `Drop`, though not under
   an already-poisoned mutex specifically — that would need a second, independent panic to poison it
   first, which none of the existing tests induce; the fix itself is inspected directly).

8. **FIXED** `[pre-existing, verified]` — busy-spin retry loop in
   `concurrent_cold_builds_single_flight_to_one_real_build` — `thread::yield_now()` added on the
   `Building`-retry branch. `crates/tessera-authz/tests/fragment.rs:333-341`. Covering test:
   `cargo test -p tessera-authz --test fragment concurrent_cold_builds_single_flight_to_one_real_build`
   — passes.

### Task 3 (control.rs, error.rs)

9. **FIXED** `[pre-existing, verified]` — `map_join_error` lacked the mirror unit test
   `map_store_error` has (I13: a panic's `Display` must never reach the response body).
   `crates/tessera-server/src/error.rs:238-260` —
   `map_join_error_does_not_forward_the_detail_to_the_caller`, spawns a task whose panic message
   names a path and an entity id, asserts neither appears in the mapped body. Covering test:
   `cargo test -p tessera-server --lib error::tests::map_join_error_does_not_forward_the_detail_to_the_caller`
   — passes.

10. **FIXED** `[pre-existing, verified; one sub-case does not apply]` — needless `Arc::clone` of
    `state` in `item`/`ingest`/`changes`/`authorise`. `ingest` (`control.rs:396-402`), `changes`
    (`control.rs:544-549`) and `item` (`viewer.rs:636-644`) now move `state` directly into the
    `spawn_blocking` closure instead of cloning, since none of the three uses `state` again after
    the `.await`. **`authorise` (`session.rs:47-84`, `tessera-server`) is unchanged** — verified
    that `state.sessions.lock().insert(session)` runs *after* the `spawn_blocking` call, so `state`
    is genuinely needed a second time there; the `Arc::clone` is not needless in the current code
    and removing it would not compile. `viewport` (`viewer.rs`) also keeps its clone for the same
    reason (`state.stage_timing` read after the closure) — it was never named in the ledger line,
    and inspection confirms it shouldn't have been. Covering tests: `cargo test -p tessera-server`
    (all 61 unit+integration tests green, including every ingest/changes/item/authorise/viewport
    path).

### Task 4 (config.rs, control.rs, state.rs, contracts-spec)

11. **FIXED** `[pre-existing, verified]` — no bound check vs `tokio::sync::Semaphore::MAX_PERMITS`
    on `compute_admission + compute_queue`. `crates/tessera-server/src/config.rs:75-84` (new
    `ConfigError::ComputeAdmissionQueueOverflow` variant + `Display` arm),
    `config.rs:509-519` (`checked_add` + bound check at parse), `config.rs:725-745`
    (`an_absurd_admission_plus_queue_refuses_to_start` test, using values that fit TOML's i64 range
    but whose sum exceeds `MAX_PERMITS`). Covering test:
    `cargo test -p tessera-server --lib config::tests::an_absurd_admission_plus_queue_refuses_to_start`
    — passes.

12. **FIXED** — `shed_total` counts gate sheds only; D-G's `ProjectionBuilding`/`FragmentBuilding`
    429s are invisible to it. Documented (not renamed — renaming is a wire-adjacent `/control/status`
    field-shape change the ledger line didn't ask for and I judged out of proportion for a
    documentation minor) at three sites: `crates/tessera-server/src/state.rs:88-98`
    (`ComputeGate::shed_total`'s doc), `state.rs:110-112` (`ComputeGateStatus::shed_total`'s doc),
    `crates/tessera-server/src/control.rs:564-569` (the `/control/status` handler's own comment).
    Covering: `cargo test -p tessera-server` green (doc-only change, no behavioural assertion to
    add); the confusion this documents is exactly what Task 9's bench report already worked through
    empirically (cited in the new doc).

13. **FIXED** — contracts §3.1 amendment embedded Rust identifiers + "D-B" in the wire doc.
    `docs/design/contracts.md:237` — trimmed to the endpoint list (`/v1/viewport`,
    `/v1/items`, `/session/authorise`, plus "per-key cold-build admission on those same endpoints")
    and `Retry-After: 1`, with `EngineError::ProjectionBuilding`/`FragmentBuilding` and the `D-B`
    label removed. Checked `docs/superpowers/plans/2026-07-28-phase1-walking-skeleton.md`'s matching
    restatement (line 99 area) — it already says "Task 4" (not "D-B") and carries no Rust
    identifiers, so it needed no edit. Covering: none (prose-only spec change); read back after
    editing to confirm it still parses as a normal markdown table row.

14. **FIXED** — `available_parallelism()` failure fallback to 1 degrades silently — startup log
    line added. `crates/tessera-server/src/config.rs:375-392` (`default_compute_threads`) —
    `tracing::warn!` on the `Err` arm, naming the OS error and pointing at `serve.compute_threads`
    as the escape hatch. (The engine-side twin, `tessera_engine::default_compute_threads`, is used
    only by embedders/tests/benches with no startup-log infrastructure to hook into and no fail-open
    startup path to guard — left unchanged, matching its own doc's stated asymmetry, item 19 below.)
    Covering: `cargo build -p tessera-server` clean; not unit-tested directly (the `Err` branch of
    `available_parallelism()` isn't triggerable in a normal test environment) — inspected by
    reading, consistent with this crate's existing convention for OS-failure branches.

### Task 5 (http.rs)

15. **SKIPPED — already fixed**, prior to this fix wave. "Server disconnect test doc — same
    checkpoint ambiguity as engine test" was explicitly recorded in the ledger as "[being addressed
    in fix round 1 alongside the Important]" and Task 5's own report documents that fix. Verified
    directly: `crates/tessera-server/tests/http.rs:2690-2699` carries the "What this test does NOT
    claim" paragraph on `dropping_a_client_connection_mid_viewport_releases_the_gate_promptly`,
    committed as part of Task 5 (commit `af3d94c`). No further action.

16. **FIXED** — `poll_until_in_flight`'s 2s cap could turn a slow-runner regression into the
    helper's own panic instead of the calling test's ratio assertion. `crates/tessera-server/tests/
    http.rs:2250-2270` — bound widened 2s → 10s (2000 → 10,000 iterations at 1ms), and the panic
    message now states explicitly that it's the helper's own generous-but-finite timeout firing,
    not necessarily the calling test's real assertion, so a future reader isn't misled about which
    mechanism failed. Both failure modes remain genuine failures either way, as the ledger notes —
    this only reduces spurious *helper* panics and clarifies the message when one does fire.
    Covering test: `cargo test -p tessera-server --test http` (35 tests, all green, including every
    caller of `poll_until_in_flight`).

### Task 6 (session.rs, viewport.rs, tests/viewport.rs, http.rs)

17. **FIXED (documentation)** — I13 pool-panic test uses a look-alike pool, not `Engine`'s own.
    `crates/tessera-engine/src/session.rs:1070-1088` — kept as an engine-internal `#[cfg(test)]`
    module test (the ledger's own explicit fallback: `Engine::pool` is `pub(crate)`, unreachable
    from the `tests/viewport.rs` integration binary, which is precisely the case that fallback
    names), but added a paragraph explaining *why* going further — opening a real `Engine` from
    inside this module instead — was considered and rejected: it would require duplicating
    `tests/viewport.rs`'s ~100-line bundle-fixture harness (`tessera_build::build` + Arrow-writing
    two extents) into `src/session.rs`, disproportionate for a test whose only claim is rayon's own
    panic-propagation guarantee. Judged this as the correct, proportionate resolution rather than a
    skip, since the ledger's own wording anticipates exactly this case. Covering test: `cargo test
    -p tessera-engine --lib session::tests::a_panic_inside_the_shared_pool_propagates_to_the_caller`
    — passes.

18. **FIXED** — byte-equality fixtures never exercised `tile_result`'s `visible == 0 -> Ok(None)`
    empty-tile skip path. Two new tests, engine and server level, reusing each file's *existing*
    fixture (no new fixture-building code): the scatter formula
    `(x,y) = ((e*37) % 1000, (e*53) % 1000)` is a bijection of `e % N` onto the 1000×1000 residue
    lattice, so at `zoom = 8` (up to 65,536 candidate tiles) the overwhelming majority of candidate
    tiles are genuinely empty while a real minority are not — the mix needed, produced purely by
    changing the requested zoom.
    - `crates/tessera-engine/tests/viewport.rs:2447-2519` —
      `viewport_output_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles`.
      Asserts `0 < out_1.tiles.len() < candidate_tiles` (both a non-empty and a skipped tile exist)
      before the byte-equality assertion.
    - `crates/tessera-server/tests/http.rs:2943-3029` —
      `viewport_response_body_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles`,
      same structure at the wire level.
    Covering tests: both run individually and green
    (`cargo test -p tessera-engine --test viewport sparse_empty_tiles`,
    `cargo test -p tessera-server --test http sparse_empty_tiles`), and again as part of the full
    suite runs below.

19. **Already satisfied, no change needed** — `EngineConfig::compute_threads = 0` falls back to
    rayon's default for direct embedders while the server refuses at parse; documented asymmetry.
    Verified `crates/tessera-engine/src/session.rs:93-101` already carries this exact documentation
    (added during Task 6's original implementation). The ledger line describes a state that was
    already true and already recorded — nothing further to fix.

20. **SKIPPED — explicitly out of scope.** `TILE_PAR_MIN_LEN = 4` argued not measured. The dispatch
    brief explicitly forbids touching `TILE_PAR_MIN_LEN` or the tile-loop parallelism threshold
    ("a separate calibration task follows this one and owns that area"). No change made.

21. **FIXED** — no comment stated the cancellation bound (`≤ compute_threads` tiles in flight) at
    the parallel sweep. `crates/tessera-engine/src/viewport.rs:632-642` — comment-only addition
    immediately above `self.pool.install(...)`, explaining that the parallel section itself doesn't
    short-circuit on a cancellation flip, so the wasted work after a flip is bounded by the number
    of workers (`compute_threads`) that were already past `tile_result`'s checkpoint, not by the
    total tile count. Verified this is a comment-only diff (no code inside the parallel-sweep region
    changed) — checked via `git diff` before committing. Covering: existing D-C cancellation tests
    (`cargo test -p tessera-engine --test viewport` — `absent_cancel_token_never_aborts`,
    `pre_flipped_cancel_token_aborts_immediately_with_no_partial_output`,
    `cancel_flipped_from_another_thread_aborts_a_long_request_before_it_completes`) all still pass
    unchanged, confirming no behavioural drift.

22. **SKIPPED** — "check-layers deny inspects normal deps depth 1 only — pre-existing script
    semantics." `scripts/check-layers.sh`'s header comment already states this is deliberate
    ("DIRECT dependencies only... `--depth 1` is load-bearing"), and the ledger's own phrasing
    ("pre-existing script semantics") frames this as an observation, not a defect. No concrete fix
    is stated or implied beyond what the script's own comment already documents. No change made.

### Task 9 (bench_concurrency.py, load.rs, task-9-report.md)

23. **FIXED** — `run_shed_cell`'s server boot omitted `admission_timeout_ms` while its own
    `hang_timeout_ms` computation a few lines later reads `args.admission_timeout_ms` — a latent
    desync if `--admission-timeout-ms` is passed alongside `--criteria shed`.
    `scripts/bench_concurrency.py:401-410` — `admission_timeout_ms=args.admission_timeout_ms` added
    to the `spawn_with_long_boot_deadline(...)` call, matching the sibling pattern already used in
    `run_panstorm_cell`. Covering: `python3 -m py_compile scripts/bench_concurrency.py` clean;
    not exercised end-to-end (would require booting a real server, out of proportion for this fix —
    the change is a one-line keyword-argument addition to an already-tested code path, verified by
    reading against `run_panstorm_cell`'s identical pattern a few lines below).

24. **FIXED** — the pan-storm report over-claimed "confirming the mechanism fired" from a signal
    (87 client-side aborts) that is equally consistent with the server simply completing each
    request before or around the same time as the client gave up (run-to-completion), not with D-C's
    server-side cancellation actually engaging. `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/
    task-9-report.md`, Criterion 5 section — reworded to state plainly what `storm_aborted` and the
    permit gauges do and do not prove, and to name the actual proof of the mechanism
    (`crates/tessera-server/tests/http.rs::dropping_a_client_connection_mid_viewport_releases_the_gate_promptly`)
    explicitly, softening the PASS verdict's basis to the throughput-ratio and no-hang claims the
    criterion is actually about. Checked `crates/tessera-bench/src/arms/load.rs`'s module doc and
    `scripts/bench_concurrency.py` for the same over-claim — neither makes it (both are factual
    counters/field-passthroughs with no "mechanism fired" language) — so only the report needed
    softening.

25. **FIXED** — `sample_cpu_mean` read `samples` (via its own `stop.set()` then immediate read)
    before `sampler.join()`, deviating from the file's established stop-join-read pattern used a
    few dozen lines above (the matrix loop's `stop.set(); sampler.join(); <read samples>`).
    `scripts/bench_concurrency.py:358-364` (`run_cpu_saturation_cell`) — reordered to `stop.set();
    sampler.join(); cpu_mean = sample_cpu_mean(...)`, matching the established pattern exactly (the
    redundant `stop.set()` inside `sample_cpu_mean` itself is now a harmless no-op, since `Event.
    set()` is idempotent). Covering: `python3 -m py_compile` clean; benign under the GIL either way
    per the ledger's own characterisation, so this is a correctness-of-pattern fix, not a bug fix
    for an observed failure.

## Totals

- **FIXED:** 20 (items 1,2,3,5,6,7,8,9,10,11,12,13,14,16,17,18,21,23,24,25)
- **Already satisfied, no change needed:** 1 (item 19)
- **SKIPPED, already fixed before this wave:** 1 (item 15)
- **SKIPPED, explicitly out of scope:** 1 (item 20, TILE_PAR_MIN_LEN)
- **SKIPPED, no actionable fix / process note:** 2 (items 4, 22)

25/25 accounted for.

## Test evidence (full runs, this session)

```
cargo build --workspace --all-targets                                   -- clean
cargo clippy -p tessera-authz -p tessera-engine -p tessera-server \
  --all-targets -- -D warnings                                          -- clean
cargo clippy -p tessera-engine --all-targets --features bench-timing \
  -- -D warnings                                                        -- clean
cargo fmt --check -p tessera-authz -p tessera-engine -p tessera-server  -- clean except 2
  pre-existing, untouched-by-this-session diffs (tessera-authz/src/single_flight.rs:162,205;
  tessera-server/src/state.rs:297) -- both already documented as pre-existing drift in Task 5/6's
  own reports, confirmed unrelated to any line this session or the prior session edited.
bash scripts/check-layers.sh                                            -- exit 0
cargo test -p tessera-authz -p tessera-engine -p tessera-server         -- all green
  (tessera-authz: 16+9+2; tessera-engine: 17+12+1+37(+1 ignored); tessera-server: 26+35)
cargo test -p tessera-engine --features bench-timing                    -- all green
  (17+12+1+37(+1 ignored))
cargo test --workspace                                                  -- all green, including
  the historically-flaky tessera-store::write_permutation_rejects_entity_id_not_fitting_u32
  (passed in 59.13s this run, no SIGKILL)
python3 -m py_compile scripts/bench_concurrency.py                      -- clean
python3 -c "import ast; ast.parse(open('scripts/bench_concurrency.py').read())"  -- clean
```

**Pre-existing, environmental, out of scope (noted, not touched):**
- `tessera-store`'s `write_permutation_rejects_entity_id_not_fitting_u32` is a documented
  OOM-SIGKILL flake under memory pressure — did not reproduce this session (passed at 59.13s).
- `cargo clippy --workspace --all-targets -- -D warnings` (the literal whole-workspace invocation)
  fails on a pre-existing `clippy::doc_lazy_continuation` lint in
  `crates/tessera-store/tests/permutation_project_parallel.rs:210-214` (Task 7's own file, never
  touched by this fix wave). Reproduced identically against a clean `git stash` of this session's
  changes (i.e. at HEAD `561977c` with zero fix-wave edits applied), confirming it predates this
  session entirely — most likely a clippy-lint version drift since that file was last reviewed.
  Verified no *other* crate has any new or pre-existing clippy issue by running clippy scoped to
  every crate this session touched (`tessera-authz`, `tessera-engine`, `tessera-server`, both
  default and `bench-timing`) plus a full `cargo clippy --workspace --all-targets -- -D warnings`
  run whose only errors are the 5 lines above, all in that one untouched file.

## Self-review

- Every "FIXED" item was checked against its own covering test (or, where none plausibly applies —
  doc-only changes, a one-line Python keyword argument, a spec-prose trim — checked by direct
  reading and, where possible, a workspace-wide green run) before being marked fixed.
- The pre-existing uncommitted work (items 1,2,3,5,6,7,8,9,10,11) was not merely trusted: each was
  re-read against its exact ledger line and its actual current code, not assumed correct because it
  was already there. Item 7 in particular looked surprising at first (the Drop-guard fix appeared
  already present in a plain `Read` of the file before I understood the file was showing *this
  session's own uncommitted diff* layered on top of committed HEAD) — resolved by diffing against
  HEAD explicitly (`git diff`) rather than trusting a single `Read`.
- Item 10 (needless `Arc::clone`) was not applied uniformly to all four named handlers — `authorise`
  was checked and found to need its clone (state used again after `spawn_blocking`), so it was left
  alone rather than force a change that would not compile. This is flagged explicitly above rather
  than silently under-delivering against the ledger's four-handler list.
- Items 4, 15, 19, 20, 22 were not treated as "nothing to do" without justification — each has a
  documented reason (no actionable fix stated, already fixed before this wave, already satisfied by
  existing code, explicitly out of scope, or a pre-existing script comment already covers it).
- No file outside the ledger's named files/areas was touched. No edit was made inside the
  `TILE_PAR_MIN_LEN`/tile-loop parallel-sweep region other than the one explicitly-sanctioned
  comment addition (verified via `git diff` on `viewport.rs` before committing).
- `.cargo/config.toml` was not touched; no isolated `CARGO_TARGET_DIR` was created; the shared
  target dir was used throughout, per the disk-space instruction.
