# Task 5 report — cooperative cancellation for the viewport path (D-C)

## Status

DONE.

## Implementation

### Engine side

**New module `crates/tessera-engine/src/cancel.rs`.** `CancelToken(Arc<AtomicBool>)`, `Clone`,
`Default`, no `tokio` dependency (engine API stays synchronous, lifecycle §7). `new()`, `cancel()`
(store), `is_cancelled()` (load). Re-exported from `lib.rs` as `tessera_engine::CancelToken`.

**Ordering: `Relaxed` for both the store and the load.** The flag carries no payload — nothing
else needs to be published alongside it or synchronised against it — so `Release`/`Acquire` would
buy ordering this token has no use for, and neither ordering bounds *when* another thread observes
the flip, only what other memory operations are ordered relative to it (there are none here).
`Relaxed` gives exactly the guarantee cooperative cancellation needs (every thread holding a clone
eventually observes a flip made on any other clone) at the lowest cost per check, which matters
because the per-tile check sits on the hot path. Documented in `cancel.rs`'s module doc; this is
the ordering the brief explicitly permits.

**`ViewportRequest`** (`crates/tessera-engine/src/viewport.rs`) gained `pub cancel: Option<CancelToken>`
and a builder setter `.cancel(Option<CancelToken>)`, matching the existing `.pin()`/
`.underlay_offset()` style.

**`EngineError::Cancelled`** new variant (`session.rs`), `Display` = `"request cancelled"`.

**Three checkpoints in `Engine::viewport`**, via a free function `check_cancelled(&Option<CancelToken>) -> Result<()>`:
1. Before `compose` — placed *after* the row-projection single-flight `get_or_build` call, so a
   cancellation observed here never interrupts that build (D-G's non-cancellable-builder rule);
   it only pre-empts work this request would otherwise go on to do itself.
2. Before θ's anchor (`mask.visible_total()`).
3. At the top of the per-tile loop, before any of that tile's count/select/gather/underlay work.

A hit at any checkpoint returns `Err(EngineError::Cancelled)` immediately — the function returns
right there, so no `ViewportOut` (partial or otherwise) is ever constructed (I13). The
single-flight row-projection/fragment builders are never threaded with the token, matching D-C's
scope note.

### Server side (`crates/tessera-server/src/viewer.rs`)

**`CancelGuard`** — a small guard struct (`token: CancelToken, armed: bool`) whose `Drop` flips
the token unless disarmed. Created in the `viewport` handler *before* `state.compute_gate.admit()`
(per the brief), as a local (`let mut cancel_guard = CancelGuard::new(cancel.clone())`) so it lives
in the handler future's own state machine — when axum drops that future on client disconnect
(the only signal this transport gives for "the client went away"), the guard drops with it.

Only a **clone** of `cancel` moves into the `spawn_blocking` closure (via `run_viewport`'s new
`cancel: CancelToken` parameter, threaded into `ViewportRequest::cancel(Some(cancel))`); the
guard keeps the original outside the closure, on the reactor.

**Disarmed on the normal path**, immediately after the `spawn_blocking` call returns successfully
and before response construction, so a completed request's own guard drop (at function return)
never flips an already-irrelevant token. (Documented as harmless either way if it *did* fire late —
the engine call has already returned by then — but disarming keeps "cancelled" meaning what it
says.)

### `map_engine_error` (`crates/tessera-server/src/error.rs`)

New **explicit** arm: `EngineError::Cancelled => ApiError::FailClosed("request cancelled before
completion; ...")` — a fixed, hand-written string, never `EngineError::Cancelled`'s own `Display`,
matching the same "never forward a lower layer's text" rule `map_store_error`/`map_join_error`
already enforce. Placed before the catch-all so it can't silently regress to whatever the
catch-all does. Comment explains this arm is unreachable today (the guard only fires when the
whole handler future is dropped, meaning nobody is left to read a response) but must stay
fail-closed-shaped in case a future refactor makes it reachable on a still-live connection.

## TDD evidence (RED/GREEN)

All three new tests were run against a temporarily neutered `check_cancelled` (`fn
check_cancelled(_cancel: &Option<CancelToken>) -> Result<()> { Ok(()) }`) to confirm genuine RED,
then restored (`diff` confirmed byte-identical to the pre-neutering file) and re-run GREEN.

**Engine (`crates/tessera-engine/tests/viewport.rs`):**

- `pre_flipped_cancel_token_aborts_immediately_with_no_partial_output` — a token cancelled before
  the call is made; asserts `Err(EngineError::Cancelled)` specifically. RED (neutered): failed,
  `got Ok(ViewportOut { ... })`. GREEN (restored): pass.
- `cancel_flipped_from_another_thread_aborts_a_multi_tile_sweep_before_it_completes` — a genuinely
  racy, self-scaling wall-clock-ratio test (same pattern this file's own D-G tests and
  `tessera-server`'s admission-gate tests already use): `zoom = 2` (16 tiles), `underlay_offset =
  8` (~1.05M sub-cell evaluations total, independent of corpus size — same cost-model trick the
  server's slow-viewport fixtures use) engineers a baseline sweep of ~270ms on this machine. A
  canceller thread whose entire job is one atomic store, released from the same `Barrier` the
  engine call starts from, races it. Measured in tuning: baseline ≈270ms, cancelled run ≈4ms —
  asserts `cancelled_elapsed < baseline_elapsed / 2` (wide margin over the observed ~70x ratio).
  RED (neutered): failed, `expected Cancelled, got Ok(...)` (full 16-tile result returned). GREEN
  (restored): pass.
- `absent_cancel_token_never_aborts` — no-cancel-token path is unaffected (regression guard,
  always green).

I considered and rejected adding a test-only hook to pause the engine mid-loop at a specific tile
(the brief explicitly calls this "not worth it"); the timing-ratio test above is the practical
substitute the brief itself suggests, and it does exercise genuine multi-tile interruption rather
than only the pre-flighted-before-any-work case.

**Server (`crates/tessera-server/src/error.rs`):**

- `map_engine_error_takes_cancelled_to_fail_closed_500` — asserts `EngineError::Cancelled` maps to
  `500`/`"fail-closed"`, not the catch-all's coincidentally-same-shaped output. (This test is GREEN
  by construction the moment the variant exists with any mapping; the meaningful RED here was
  `EngineError::Cancelled` not existing at all until `session.rs` was edited — confirmed via
  `cargo build` failing with "no variant `Cancelled`" before that edit landed.)

**Server (`crates/tessera-server/tests/http.rs`):**

- `dropping_a_client_connection_mid_viewport_releases_the_gate_promptly` — see "how disconnect
  propagation was tested" below. RED (neutered engine check): failed —
  `the gate took 801.233637ms to free its permit after the client disconnected, not meaningfully
  less than the 826.852667ms an uncancelled sweep takes on this run`. GREEN (restored): pass
  (~0.95–1.0s wall time for the whole test).

## How disconnect propagation was tested

**First attempt used the file's existing `slow_viewport_body()` fixture (`zoom = 0`, one tile,
`underlay_offset = 12`) and failed even with the real implementation wired in** — release time
matched the baseline almost exactly (~4.3s both). Root cause: D-C's per-tile check sits at the
*top* of the tile loop, deliberately not mid-tile (a tile's own underlay sweep is bounded,
in-flight work like every other per-tile stage). With `zoom = 0` there is exactly one tile, so the
*entire* slowness lives inside that one tile's underlay sweep — cancellation could only ever be
observed once that whole sweep had already finished, indistinguishable from no cancellation at
all. This was a test-fixture bug, not an implementation bug (confirmed by the engine-level RED/GREEN
tests already passing correctly against the same code).

Fixed by adding a **new fixture** (`slow_multi_tile_viewport_body()`): `zoom = 2` (16 tiles),
`underlay_offset = 9` (~4.2M sub-cell evaluations spread across 16 tiles, ~800ms–1.1s baseline on
this machine), so a disconnect landing after any prefix of tiles releases the gate long before the
rest would have run.

**Disconnect mechanism.** `slow_task.abort()` on the `tokio::spawn`ed task driving the client's
`reqwest` request. I verified this is a faithful stand-in for a real browser aborting a stale fetch
with two standalone probes (not part of the committed test suite, run manually against axum 0.8 /
hyper 1.x / reqwest 0.12, the same versions this workspace pins):

1. A GET handler that `tokio::time::sleep`s 3s, wrapped in a `Drop`-flag guard: aborting the
   client task flips the flag within ~1ms of the abort call — the handler future is dropped
   promptly on disconnect.
2. The same shape but with the handler awaiting a CPU-bound `spawn_blocking` closure (mirroring
   this task's real structure): same result — the *outer* handler future still drops promptly even
   while the blocking closure keeps running on its own OS thread (as it must — `spawn_blocking`
   tasks are not killable via `JoinHandle` drop). This confirms the design's premise: dropping the
   handler future flips the guard; only the engine's own per-tile check inside the closure can
   actually shorten the closure's own runtime.

**Test structure** (warm-session scope, per the brief): a fast warm-up request first (same token)
brings the row-projection cache to `Ready`, so neither the baseline nor the cancelled run's timing
includes the (non-cancellable) cold-build cost. `baseline_elapsed` is a full, uncancelled
multi-tile sweep's measured wall time on this run. The scenario then starts a second slow request,
polls `/control/status` until `compute.in_flight == 1` (deterministic, not a guessed delay,
reusing the `poll_until_in_flight` helper Task 4 already established), aborts it, and measures
`release_elapsed` from the abort to `in_flight` returning to `0` (same poll helper). Asserts
`release_elapsed < baseline_elapsed / 2`, and — independently — that a follow-up request is
admitted with `200` rather than shed `429` (proving the permit didn't leak).

## Files changed

- `crates/tessera-engine/src/cancel.rs` — new: `CancelToken`, ordering doc, 3 unit tests
- `crates/tessera-engine/src/lib.rs` — `pub mod cancel;` + re-export
- `crates/tessera-engine/src/session.rs` — `EngineError::Cancelled` + `Display` arm
- `crates/tessera-engine/src/viewport.rs` — `ViewportRequest.cancel` + setter, `check_cancelled`,
  three checkpoints
- `crates/tessera-engine/tests/viewport.rs` — 3 new tests + `config_for_slow_multi_tile_sweep`
  helper
- `crates/tessera-server/src/viewer.rs` — `CancelGuard`, handler wiring, `run_viewport` signature
- `crates/tessera-server/src/error.rs` — explicit `Cancelled` arm + unit test
- `crates/tessera-server/tests/http.rs` — `slow_multi_tile_viewport_body` fixture + 1 new
  end-to-end test

## Verification

```
cargo test -p tessera-engine -p tessera-server
  tessera-engine lib:      (no tests)
  tessera-engine tests/viewport.rs: 34 passed; 0 failed; 1 ignored
  tessera-server lib:      24 passed; 0 failed
  tessera-server tests/http.rs: 33 passed; 0 failed

cargo test --workspace          -> exit 0 (full run, including the known
                                    tessera-store OOM-flake test, which
                                    passed this run in 185.90s — noted as
                                    pre-existing/environmental per the brief,
                                    not touched)
cargo clippy -p tessera-engine -p tessera-server --all-targets -- -D warnings  -> clean
cargo fmt --check (on files this task touched)                                -> clean
bash scripts/check-layers.sh                                                  -> exit 0
```

## Self-review

- Confirmed the three checkpoints match the brief's exact list (per-tile top-of-loop; before
  compose; before θ's anchor) and are placed via `git diff` inspection, not just by reading my own
  summary.
- Confirmed the row-projection build is genuinely outside the guarded region — `check_cancelled`
  is called *after* `get_or_build` returns, so a cancelled request that raced a cold build still
  lets that build run to completion (matches D-C's "not cancellable" scope note); did not add a
  dedicated test for this because it would be testing D-G's own (already-tested) single-flight
  behaviour rather than anything this task changes.
- Confirmed `CancelGuard`'s fields are private and its only public surface is `new`/`disarm`, so a
  caller can't accidentally construct one pre-armed-wrong or forget to disarm via a typo on a
  public field.
- Confirmed only a *clone* of the token crosses into the `spawn_blocking` closure — `cancel_guard`
  itself is never moved, so its `Drop` can only ever run on the reactor side (where axum's future
  drop is the trigger), never inside the blocking closure.
- Verified via two standalone axum/hyper/reqwest probes (see "How disconnect propagation was
  tested") that the disconnect-detection mechanism this design relies on is real, not assumed —
  this caught a test-fixture bug (single-tile fixture reused from Tasks 3/4 cannot exercise
  per-tile interruption at all) before it could be mistaken for an implementation bug.
- Re-ran the full RED/RED/GREEN cycle for both the engine-level and server-level disconnect tests
  after fixing the fixture, to be sure the final GREEN wasn't accidentally passing for an unrelated
  reason.
- `EngineError::Cancelled`'s `Display` (`"request cancelled"`) is never used by the server's
  mapping (`map_engine_error` writes its own fixed string) — confirmed by reading the arm, not
  merely by the "no lower-layer text" test passing, since a test asserting *absence* of a specific
  substring wouldn't catch every way `Display` output could leak.

## Concerns

- The server-side end-to-end disconnect test is inherently a wall-clock-ratio test (same category
  as `healthz_stays_prompt_while_a_long_viewport_runs` and the Task 4 gate tests already in this
  file) — genuinely fast/robust on this machine (~1s total, ~70x+ margin between baseline and
  cancelled-run timings in tuning), but, like its siblings, could in principle need retuning on a
  much slower or more contended CI runner. No sleep-based synchronisation was used anywhere
  (admission is polled via `/control/status`, not guessed).
- `CancelToken::cancel`/`is_cancelled` use `Relaxed` ordering as documented; this is a *cooperative*
  best-effort mechanism by design (I13 requires the abort to happen, not to happen within any
  specific bound), so no stronger ordering was judged necessary. Flagged here only so the ordering
  choice gets a second look from a reviewer with fresher eyes on this specific tradeoff.
- I did not add a dedicated engine-level test proving the pre-compose checkpoint is placed *after*
  (not before) the row-projection build — this is implicit in the code (`check_cancelled` call
  site is textually after `get_or_build`) and stated in this task's self-review, but there is no
  automated regression guard against a future edit accidentally moving it earlier. Judged
  acceptable because Task 1/2's own D-G tests would need updating too if that boundary moved in a
  way that broke single-flight semantics, giving indirect coverage.

## Fix round 1 (review: Important, viewport.rs:558-561/573, http.rs:969)

**Finding.** `cancel_flipped_from_another_thread_aborts_a_multi_tile_sweep_before_it_completes`'s
doc claimed the per-tile check was what the test exercised ("evidence that the per-tile check
actually interrupts in-flight work"). That claim is false as written: the cancelled run's session
was deliberately fresh (a cold row-projection cache key), so the engine call still has to pay the
row-projection build cost before reaching even the *first* checkpoint (pre-compose). The
canceller thread's entire job is one atomic store released from the same barrier, which lands
microseconds after release — essentially always before the cold build finishes, so in practice
the flip is caught at the pre-compose checkpoint, before any tile runs at all. Deleting the
per-tile check entirely would leave this test green. My report's claim ("it does exercise genuine
multi-tile interruption rather than only the pre-flighted case") repeated the same overclaim. The
D-C brief explicitly sanctions relying on code review for per-tile placement rather than adding
test-only hooks — so this was not a spec violation, but the doc and my report described the test
as proving something it does not prove, which would mislead a future maintainer.

The same defect applies to the server-level test
(`dropping_a_client_connection_mid_viewport_releases_the_gate_promptly`,
`crates/tessera-server/tests/http.rs`): `poll_until_in_flight(&server, 1)` only proves the request
has been admitted and started running compute, not how far into the sweep it has gotten by the
time `slow_task.abort()` fires — the disconnect could equally land at the pre-compose checkpoint.

**Fix — option (b) plus honest doc, chosen over (a) alone.** I warmed the cancelled run's session
before racing it: one cheap prior `Engine::viewport` call (`zoom = 0`, no underlay — builds the
row projection without doing any of the expensive underlay sweep) against the *same* session,
before the barrier + flip. This removes the one genuinely slow, uncontrolled stage
(row-projection build) that could precede the checkpoints regardless of anything this task
controls, so the remaining pre-tile-loop work (generation load, pin check, view lookup, a cache
*hit*, `compose`, `visible_total`) is cheap, in-process, sub-tile-length work — biasing the flip
toward landing during the 16-tile loop rather than guaranteeing it. I chose (b) over (a) alone
because it strictly improves the test's evidentiary value (a warmed race is closer to what a real
rapid-pan looks like — a session that has already drawn at least one viewport) at zero cost to
determinism: the assertions (`Err(Cancelled)`, `cancelled_elapsed < baseline_elapsed / 2`) already
held regardless of which checkpoint fired, so warming changes which checkpoint is more likely to
be exercised without changing the pass/fail logic or introducing any new source of flakiness.

Paired with this, I rewrote both docs to state plainly what the test does and does not prove:

- Renamed the engine test to
  `cancel_flipped_from_another_thread_aborts_a_long_request_before_it_completes` (dropping
  "multi_tile_sweep" from the name, since the test no longer claims to pin down the tile loop
  specifically) and added a "What this test does NOT claim" paragraph: it does not assert or
  reliably force which checkpoint catches the flip; warming only biases, does not guarantee;
  per-tile placement is a code-review concern per the D-C brief, not something this test proves.
- Added the equivalent "What this test does NOT claim" paragraph to the server test's doc,
  stating that `poll_until_in_flight` only proves admission/running, not sweep progress, and that
  the test's value is observing end-to-end permit release, not proving the per-tile check
  specifically fires mid-sweep.

I did not touch the assertions themselves in either test — both were already checking only the
externally-observable contract (whole-request abort, well before the full sweep's time), never a
specific checkpoint, so no assertion was overclaiming; only the doc comments and this report's
prose were.

**Withdrawn claim.** My original report's TDD-evidence section said the engine timing test "does
exercise genuine multi-tile interruption rather than only the pre-flighted-before-any-work case".
That sentence is false as the test was written at the time (fresh/cold session) and remains
unproven even after the warm-up fix (biased-toward, not guaranteed-to-be, mid-loop). Withdrawn;
the accurate claim is: the test proves cancellation aborts a long-running request well before its
natural completion time, without pinning down which of the three checkpoints did the catching.

**Covering tests re-run.**

```
$ cargo test -p tessera-engine --test viewport
test result: ok. 34 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.31s

$ cargo test -p tessera-server --test http dropping_a_client_connection -- --nocapture
test dropping_a_client_connection_mid_viewport_releases_the_gate_promptly ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 32 filtered out; finished in 0.95s

$ cargo test -p tessera-engine -p tessera-server
  (all suites) ok. 20+17+12+1+34+24+33+0+0 passed; 0 failed across all binaries

$ cargo clippy -p tessera-engine -p tessera-server --all-targets -- -D warnings
Finished, clean

$ cargo fmt --check -p tessera-engine -p tessera-server
clean on every file this fix touched (one pre-existing, unrelated diff remains in
crates/tessera-server/src/state.rs, not part of this task or this fix)

$ bash scripts/check-layers.sh
exit=0
```

**Files changed (this fix round).**

- `crates/tessera-engine/tests/viewport.rs` — renamed the timing test, rewrote its doc, added a
  warm-up call for the cancelled run's session
- `crates/tessera-server/tests/http.rs` — added the "what this test does NOT claim" doc paragraph
  (no code/assertion change)
- `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/task-5-report.md` — this fix round
