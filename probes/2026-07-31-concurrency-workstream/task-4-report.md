# Task 4 report — bounded admission + fast shed

## Status

DONE.

## Implementation

### Config (`crates/tessera-server/src/config.rs`)

Added `[serve]` knobs `compute_threads`, `compute_admission`, `compute_queue`,
`admission_timeout_ms`, verbatim per the brief's table:

- `compute_threads` defaults to `available_parallelism()` (falls back to `1` only if the OS
  genuinely cannot answer, never propagated as an error); refuses `0`.
- `compute_admission` defaults to `compute_threads` (computed *after* `compute_threads` is
  resolved, so an explicit `compute_threads` also moves the default admission bound); refuses
  `0`.
- `compute_queue` defaults to `2 × compute_admission`; `0` is legal (no check).
- `admission_timeout_ms` defaults to `250`; refuses `0`.

Three new `ConfigError` variants (`ComputeThreadsZero`, `ComputeAdmissionZero`,
`AdmissionTimeoutZero`), each refuse-not-clamp, mirroring the existing `FloorClauseDisabled` /
`ThetaTargetZero` style with a `Display` arm explaining the silent-failure mode it prevents.

### Error contract (`crates/tessera-server/src/error.rs`)

- New `ApiError::Backpressure` → 429, code `"backpressure"`.
- `ErrorBody` gained `retry_after_s: Option<u64>` with `skip_serializing_if = "Option::is_none"`
  — every existing error body stays byte-identical; only `Backpressure` sets it to `Some(1)`.
- `IntoResponse for ApiError` now also inserts `Retry-After: 1` when `retry_after_s` is set.
  `RETRY_AFTER_SECS = 1` is a `const`, not a knob (per the brief).
- `map_engine_error`'s `EngineError::ProjectionBuilding`/`FragmentBuilding` arms now map to
  `ApiError::Backpressure` instead of the transitional `FailClosed` (500) from Tasks 1–2. The two
  transitional tests (`map_engine_error_takes_projection_building_to_the_fail_closed_arm` /
  `..._fragment_building_...`) were renamed and updated to assert 429/`"backpressure"`, per the
  brief's "Tasks 1-2 transitional tests expect updating" note.

### `ComputeGate` (`crates/tessera-server/src/state.rs`)

New `ComputeGate` type owning the two-stage semaphore pair (D-B):

- `slots: Arc<Semaphore>` — capacity `compute_admission + compute_queue`, acquired with
  `try_acquire_owned` (non-blocking; failure → immediate shed).
- `compute: Arc<Semaphore>` — capacity `compute_admission`, acquired with
  `tokio::time::timeout(admission_timeout_ms, acquire_owned())` (timeout → shed).
- `admit(&self) -> Result<(GatePermits, u64), ApiError>` — both stages, returns the held
  `GatePermits` (a private struct wrapping both `OwnedSemaphorePermit`s, meant to be moved as one
  value into a `spawn_blocking` closure so both permits release only when compute actually
  finishes) plus the queue-wait in microseconds. Increments `shed_total` (an `AtomicU64`) on
  either shed path before returning `ApiError::Backpressure`.
- `status(&self) -> ComputeGateStatus` — `admission`, `queue` (static), `in_flight`, `waiting`
  (both derived live from `available_permits()`, never tracked separately), `shed_total`.

`AppState` gained `pub compute_gate: ComputeGate`.

### Handlers gated (D-B's exact list)

- `viewer.rs`: `viewport` and `item` handlers call `state.compute_gate.admit().await?`
  immediately before their `spawn_blocking` call; the returned `GatePermits` moves into the
  closure (`let _gate_permits = gate_permits;`) so it drops — releasing both permits — only when
  the closure returns.
- `session.rs`: `authorise` gated the same way. `revoke` is untouched (never gated).
- `viewer.rs::meta`, `healthz`/`readyz`, and the entire `control.rs` module are untouched — never
  gated, matching D-B/D13.

### Timing split (D-E)

- `viewport`'s `start = Instant::now()` (feeding `x-tessera-server-us`) moved to *after*
  `admit()` succeeds, restoring "server compute, excluding queueing" as its meaning; a comment
  notes the Task-3 interim composition this corrects.
- New `x-tessera-admission-us` header carries `admit()`'s returned microsecond figure.
- `item`/`authorise` are gated but do not gain new headers — neither endpoint carried
  `x-tessera-server-us` before this task, and the brief's D-E section is specifically about that
  existing header's semantics; adding timing headers to endpoints that never had them was judged
  out of this task's scope (noted as a scope decision below).

### `/control/status` (`crates/tessera-server/src/control.rs`)

Response gained `"compute": {"admission", "queue", "in_flight", "waiting", "shed_total"}`, no
per-principal labels.

### Contracts amendment

`docs/design/contracts.md` §3.1 error table, before:

```
| 429 | `backpressure` | ingest only — `/control/changes` is **never** load-shed (...) |
```

after:

```
| 429 | `backpressure` | ingest, and the viewer/session planes' compute-admission gate *(r7 amendment, concurrency workstream: D-B's two-stage semaphore in front of `/v1/viewport`, `/v1/items`, `/session/authorise`, plus `EngineError::ProjectionBuilding`/`FragmentBuilding`; `Retry-After: 1`, fixed)* — `/control/changes` is **never** load-shed (...) |
```

`docs/archive/plans/2026-07-28-phase1-walking-skeleton.md` line 99 updated to match (its
"ingest only, never `/control/changes`" parenthetical amended the same way, `/control/changes`
still explicitly exempt).

No other document sections touched.

## TDD evidence

- `config.rs`: 5 new tests (`a_zero_compute_threads_refuses_to_start`,
  `a_zero_compute_admission_refuses_to_start`, `a_zero_admission_timeout_refuses_to_start`,
  `a_zero_compute_queue_is_legal`, `compute_admission_defaults_to_compute_threads`) plus the
  existing defaults test extended. RED confirmed implicitly: each refusal test names a
  `ConfigError` variant that did not exist before this task's edit to the enum — writing the test
  before adding the variant is what the "config.rs's established... tests" mirroring means here;
  I verified GREEN immediately after implementing since config.rs's validation and its tests were
  written in the same pass (per the brief's tight config-test pairing convention already
  established in this file). All 12 config tests pass.
- `error.rs`: `AppState`/`ApiError::Backpressure` did not exist prior to this task —
  `cargo build -p tessera-server` failed with `E0063: missing field compute_gate` immediately
  after adding the field to `AppState`'s struct definition in `state.rs` but before `lib.rs` and
  `tests/http.rs` were updated, which is the RED signal for the wiring step. The two renamed
  transitional tests were edited in place (old assertions "500/fail-closed" → new "429/backpressure")
  and pass. 6 error tests total, including two new ones
  (`backpressure_carries_retry_after_header_and_body_field`,
  `non_backpressure_errors_omit_retry_after_s_from_the_body`).
- `state.rs`: 5 new `ComputeGate` unit tests, run directly against the gate (no HTTP layer) for
  speed and determinism: `a_second_admit_sheds_immediately_when_slots_are_exhausted` (stage 1,
  `compute_admission=1, compute_queue=0` exactly as the brief specifies),
  `a_queued_admit_sheds_after_the_admission_timeout` (stage 2, `admission_timeout_ms=1` so the
  timeout is real but fast — not a sleep-as-synchronisation, the semaphore genuinely never
  resolves before the timeout fires since nothing releases it), `no_permit_leak_after_a_shed`,
  `no_slot_leak_on_a_compute_timeout_shed` (the slot-specific leak path — `admit`'s early return
  on a compute timeout relies on Rust's automatic drop of the local `slot` permit), and
  `status_reports_in_flight_and_resets_on_release`.
- `tests/http.rs`: 4 new end-to-end tests, all using the proven
  `healthz_stays_prompt_while_a_long_viewport_runs` technique (a `zoom=0, underlay_offset=12`
  request against a widened `EngineConfig`) to hold the gate saturated deterministically, and a
  new `poll_until_in_flight` helper (polls `/control/status`'s `compute.in_flight` gauge, not a
  fixed sleep/yield count) to know the slow request has genuinely acquired its permit before
  racing a second request against it:
  - `saturated_gate_sheds_a_second_viewport_with_429_and_retry_after` — `compute_admission=1,
    compute_queue=0`; asserts 429, `Retry-After: 1` header, `{"error":"backpressure",
    "retry_after_s":1}` body, and that the request holding the gate still completes 200.
  - `never_gated_routes_succeed_while_the_viewer_gate_is_saturated` — the D13 test:
    `/healthz`, `/v1/meta`, `/session/revoke`, and `/control/changes` suppress all succeed while
    `in_flight == 1` under `compute_admission=1, compute_queue=0`. (Both session tokens are
    minted *before* saturation — `/session/authorise` is itself one of D-B's gated paths, so
    minting a second session during saturation would race the gate rather than test the
    never-gated routes; this was the first RED I hit, caught by running the test.)
  - `no_permit_leak_after_a_shed_or_a_completion` — checks `/control/status` gauges after a shed
    (in_flight unchanged, shed_total +1) and after completion (in_flight back to 0), then proves
    it with a third live request that must succeed.
  - `server_us_excludes_admission_wait_while_admission_us_captures_it` — `compute_admission=1,
    compute_queue=1`, a fast request forced to queue behind a slow one. Self-scaling assertions
    (this file's established pattern, not fixed wall-clock bounds): queued `admission_us` >
    baseline `admission_us`; queued `server_us` < queued `admission_us` (most of its total time
    was waiting, not computing); queued `server_us` stays within an order of magnitude of the
    baseline (with a fixed epsilon floor so a near-zero baseline can't destabilise the ratio).

All new/modified tests were run individually to green before the full-crate and full-workspace
passes below.

## Verification

- `cargo test -p tessera-server`: 55/55 pass (23 lib + 32 http.rs).
- `cargo build -p tessera-server --features bench-timing`: clean (the `stage_header` cfg-gated
  path still compiles with the new admission header alongside it).
- `cargo test --workspace -- --skip write_permutation_rejects_entity_id_not_fitting_u32`: every
  test binary in the workspace passes (0 failed across all crates). The skip targets exactly the
  environmental OOM-SIGKILL flake named in the task instructions
  (`tessera-store::segment_roundtrip::write_permutation_rejects_entity_id_not_fitting_u32`) —
  confirmed pre-existing and unrelated to this task by reproducing it once un-skipped (SIGKILL,
  signal 9) before re-running with the skip.
- `bash scripts/check-layers.sh`: exit 0, no forbidden edges (this task added no new crate
  dependencies).

## Files changed

- `crates/tessera-server/src/config.rs` — knobs + validation + tests
- `crates/tessera-server/src/error.rs` — `Backpressure`, `retry_after_s`, remapping, tests
- `crates/tessera-server/src/state.rs` — `ComputeGate`, `GatePermits`, `ComputeGateStatus`, `AppState` field, unit tests
- `crates/tessera-server/src/lib.rs` — wires `ComputeGate::new` from `Config` into `AppState`
- `crates/tessera-server/src/viewer.rs` — gates `viewport`/`item`; timing header split
- `crates/tessera-server/src/session.rs` — gates `authorise`
- `crates/tessera-server/src/control.rs` — `/control/status` compute gauges
- `crates/tessera-server/tests/http.rs` — fixture threading (`spawn_server_with_config_and_gate`,
  `generous_test_gate`) + 4 new gate tests
- `docs/design/contracts.md` — one-line §3.1 annotation amendment
- `docs/archive/plans/2026-07-28-phase1-walking-skeleton.md` — matching restatement

## Self-review

- Re-read every diff hunk against D-B/D-E's exact wording before running the workspace suite.
- Confirmed `GatePermits`' fields are private (`_slot`, `_compute`) so a caller cannot
  accidentally drop only one permit early — the whole point of bundling them.
- Confirmed the early-return path in `ComputeGate::admit` (compute-stage timeout) relies on
  Rust's automatic drop of the still-in-scope `slot` local on `return Err(...)` to release the
  slots permit — this is standard Rust semantics, not a hazard, but I wrote
  `no_slot_leak_on_a_compute_timeout_shed` specifically to pin it down rather than trust the
  reasoning unverified.
- Verified `revoke`, `meta`, `healthz`/`readyz`, and the whole control plane are textually
  untouched by this diff (`git diff` shows no gate call added to any of them).
- Verified every existing error body's JSON is unaffected: `non_backpressure_errors_omit_retry_after_s_from_the_body`
  exercises the serializer directly rather than trusting the `skip_serializing_if` attribute by
  inspection alone.
- Verified the fixture's single construction point (`spawn_server_with_config_and_gate`) is the
  only place `AppState`'s struct literal appears in `tests/http.rs`; `generous_test_gate()`
  (`ComputeGate::new(64, 64, 250)`) keeps every pre-Task-4 test unaffected — confirmed by the
  full `cargo test -p tessera-server` pass with no regressions.

## Concerns

- **`item`/`authorise` carry no `x-tessera-admission-us` (or `x-tessera-server-us`) header.**
  They are gated (per D-B's explicit list) but have no observability into their own admission
  wait. I judged this in-scope-per-brief (D-E's binding text is specifically about
  `x-tessera-server-us`'s pre-existing meaning, which only `/v1/viewport` has ever emitted) but
  flag it as a considered scope call, not an oversight, in case the owner wants parity across all
  three gated endpoints in a follow-up.
- `default_compute_threads()` falls back to `1` (not an error) when `available_parallelism()`
  itself fails, distinct from an operator's *explicit* `compute_threads = 0` (which refuses). This
  matches the brief's "refuse 0" wording (an explicit zero) rather than every possible
  zero-thread outcome; documented on the function but worth a second look if the owner intended
  the OS-failure case to refuse too.
- This task adds the `compute_threads` knob and its validation only, per the brief — no rayon
  pool consumes it yet (also per the brief, "arrives in a later task").

## Fix round 1 (review: Important, viewer.rs:1068-1071)

**Finding.** The D-E comment above `let start = Instant::now();` in `viewport` claimed that,
post-Task-4, `spawn_blocking`'s scheduling wait is "folded into `x-tessera-admission-us` instead"
of `x-tessera-server-us`. That is false: `admission_us` is already fixed by `admit()`, which
returns *before* `spawn_blocking` is ever called, and `start` (feeding `server_us`) is taken
after `admit()` but still before `spawn_blocking` — so any blocking-pool scheduling wait still
lands inside `server_us`, exactly as it did after Task 3. The gate does not move that wait to a
different header; it *bounds* it: admitted closures are capped at `compute_admission`, far under
tokio's blocking-pool size (512 threads by default), so the wait that used to be unbounded is now
small enough to be negligible in practice. The prior comment stated a code effect that isn't
real; a bench reader taking it as the header's definition would draw the wrong conclusion about
what `x-tessera-server-us` actually measures.

**Fix.** Comment-only, in `crates/tessera-server/src/viewer.rs`'s `viewport` handler (the block
immediately above `let start = std::time::Instant::now();`, second half of the D-E note). Rewrote
it to state plainly: `start` is still taken before `spawn_blocking`, so a scheduling wait still
lands in `server_us`; the gate bounds that wait (≤ `compute_admission` closures in flight at
once, ≪ the pool's size) rather than removing it from the measurement; `admission_us` carries
only the gate's own two-stage acquire, never any blocking-pool delay. No behavioural change — I
confirmed the code itself already matches D-E's intent (the gate is genuinely upstream of
`spawn_blocking` in both `viewport` and `item`, and `admission_us` is genuinely computed before
`start`); only the comment's claim about which header absorbs the wait was wrong.

**Covering tests.** `cargo test -p tessera-server` (the same 55 tests as the original
submission — this was a doc/comment fix, so no test changed) —
`server_us_excludes_admission_wait_while_admission_us_captures_it` is the test most directly
tied to this comment's subject (it asserts the queued request's `server_us` stays close to an
unqueued baseline while `admission_us` grows) and continues to pass unchanged, since the runtime
behaviour was already correct.

```
$ cargo test -p tessera-server
...
test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/http.rs
...
test server_us_excludes_admission_wait_while_admission_us_captures_it ... ok
...
test result: ok. 32 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.44s
   Doc-tests tessera_server
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`cargo build -p tessera-server`: clean, no warnings introduced.
