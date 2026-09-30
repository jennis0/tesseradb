# Admission gate default retune — `compute_admission = 4 × compute_threads`

2026-07-31, 12-core WSL2 box, 2.42M-row `categories-subclass` fixture (same box/fixture as the
calibration report). Branch `concurrency/viewpath` at `d03e46c`.

## 1. The change

`compute_admission`'s default was `compute_threads` (1×, "one CPU-bound request per core"). Per
the controller's brief, requests at this corpus scale are ~0.3 ms and mostly memory-bound, so a
1× gate under-drives the machine: `compute_admission` bounds in-flight *requests*, not runnable
CPU — the rayon pool (`compute_threads`) still bounds the parallel-sweep CPU any one admitted
request may fan out across, and the serialise phase can legitimately oversubscribe past
`compute_threads` because small requests are latency-bound on scheduling, not CPU.

`crates/tessera-server/src/config.rs`:
- New `COMPUTE_ADMISSION_MULTIPLIER: usize = 4` constant, with a provenance comment (argument +
  this report's own numbers, brief).
- `compute_admission`'s default is now `compute_threads.checked_mul(COMPUTE_ADMISSION_MULTIPLIER)`
  — `checked_mul`, not a bare `*`, because release builds run with overflow checks off and an
  unchecked multiply would silently wrap to a small, wrong permit count for an operator-supplied
  `compute_threads` extreme enough to overflow. A new `ConfigError::ComputeAdmissionDefaultOverflow`
  variant refuses to start instead (Display arm + doc comment added), matching the file's existing
  refuse-not-clamp discipline and mirroring `ComputeAdmissionQueueOverflow`'s checked-add one step
  downstream.
- `compute_queue`'s default (`2 × compute_admission`, unchanged formula) is now effectively `2 ×
  COMPUTE_ADMISSION_MULTIPLIER = 8×` cores — stated plainly in its field doc.
- The existing `compute_admission + compute_queue` checked-add / `MAX_PERMITS` bound
  (`ComputeAdmissionQueueOverflow`) is untouched and still holds: it operates on whatever
  `compute_admission` ends up being (default-derived or explicit), so the larger default does not
  bypass it — confirmed by `an_absurd_admission_plus_queue_refuses_to_start` still passing
  unmodified.

## 2. Comment/doc updates (the honest new argument)

Every place arguing the old "one CPU-bound request per core" default now argues: admitted requests
bound in-flight *requests*, not runnable CPU; `compute_threads` still bounds the parallel-sweep CPU;
the serialise phase deliberately oversubscribes up to `compute_admission` because small requests are
latency-bound on scheduling, not CPU; 4× is measured (§3 below) to recover a meaningful share of
small/moderate-request closed-loop throughput without blowing past a bounded p99 tail.

- `crates/tessera-server/src/config.rs`: `COMPUTE_ADMISSION_MULTIPLIER`'s own doc (full argument +
  numbers); `Config::compute_admission`/`compute_queue` field docs; the
  `compute_admission_defaults_to_compute_threads` test's doc comment.
- `crates/tessera-server/src/state.rs`: `ComputeGate`'s struct doc rewritten from "DEFINED as a
  bound on *runnable* CPU work — one request per core" to the requests-not-CPU argument, pointing
  at the config constant's doc for the measurement.
- `crates/tessera-server/src/lib.rs`: `prepare()`'s comment on `EngineConfig::compute_threads` no
  longer says "One number, one meaning" (false now that `compute_admission` and `compute_threads`
  diverge by 4×) — rewritten to state the two knobs bound different things and that admission
  deliberately oversubscribes the rayon pool during the serialise phase.
- `scripts/bench_k_sweep.py`: `write_config`'s docstring stated the *exact* old default relationship
  (`compute_admission = compute_threads`) as a factual claim about "the server's own defaults" —
  not in the brief's explicit list, but left uncorrected it would be a false statement about
  today's behaviour to anyone reading this script, so it was updated to name the new multiplier and
  the retune date.
- **Left alone, deliberately**: `crates/tessera-bench/src/arms/load.rs`'s "`compute_admission`
  (default = available cores, 12 here)" — this sentence is inside a dated, headed historical section
  (`# Task 9 re-measurement, 2026-07-30, ...`) reporting that specific run's own config truthfully;
  it is not a general claim about "the current default" the way the other five sites were.
  `crates/tessera-server/tests/http.rs`'s comments at `compute_admission=1, compute_queue=0` are
  explicit overrides for deterministic gate-saturation tests, not defaults — brief flagged these as
  not needing changes, confirmed by reading them.
  `bench/README.md` does not state the default anywhere (checked; no match).

## 3. Validation run — Arm A/B matrix, c=5/100/1000

Command (fixture and venv both survived from the calibration task; rebuilt nothing):

```
reference/.venv/bin/python3 scripts/bench_concurrency.py \
  --bundle /tmp/tessera-2m4 --scale 2422486 --label-set categories-subclass \
  --criteria matrix --concurrency 5,100,1000 --arms B,A \
  --w 10 --k 30 --zoom 8 --duration 10 \
  --run-dir .superpowers/sdd/i-d-like-you-to-jiggly-cupcake/bench-runs/admission-4x
```

Raw console output: `bench-runs/admission-4x-console.txt`; JSON: `bench-runs/admission-4x/concurrency-summary.json`.
Server booted with resolved `compute_threads = 12` (this box), so `compute_admission = 48`,
`compute_queue = 96`.

| arm | conc | rps (new, 4×) | rps (calib, 1×) | rps (pre-branch F4) | p50 (new) | p99 (new) | p99 (calib) | p99 ratio (new/calib) | shed% (new) | shed% (calib) | pts/s (new) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| B | 5    | **11,515** | 11,771 | 15,277 | 0.35 ms | 1.33 ms | 1.30 ms | 1.02× | 0.01% | 0.01% | 3,979,585 |
| B | 100  | **35,448** | 31,248 | 48,588 | 2.56 ms | 7.12 ms | 5.27 ms | 1.35× | 0.08% | 40.9% | 12,340,516 |
| B | 1000 | **32,645** | 28,929 | 49,475 | 25.83 ms | 82.83 ms | 63.58 ms | 1.30× | 1.88% | 39.6% | 11,343,504 |
| A | 5    | **10,656** | 11,039 | — | 0.37 ms | 1.38 ms | 1.33 ms | 1.04× | 0.0% | 0.0% | 4,311,299 |
| A | 100  | **32,644** | 27,694 | — | 2.74 ms | 7.93 ms | 5.61 ms | 1.41× | 0.0% | 48.2% | 12,728,582 |
| A | 1000 | **24,700** | 20,966 | — | 27.36 ms | 93.45 ms | 63.36 ms | 1.47× | 35.16% | 60.0% | 9,724,471 |

(Arm A has no pre-branch F4 baseline — the F4 memo only ran Arm B; calibration numbers for Arm A
are from `calibration-report.md` §6.)

**Reading it honestly, per the brief's own instruction not to retune further:**

- **p99 stayed bounded everywhere** — worst ratio 1.47× (Arm A c=1000), comfortably inside the
  ~2× ceiling the brief set as the "something is wrong" line. No cell needs flagging on this axis.
- **c=100 and c=1000 show a real, substantial drop in shed rate** and a genuine rps gain: Arm B
  shed% collapsed from 40.9%→0.08% (c=100) and 39.6%→1.9% (c=1000); Arm A similarly from
  48.2%→0.0% and 60.0%→35.2%. rps rose ~13% (B c=100), ~13% (B c=1000), ~18% (A c=100), ~18%
  (A c=1000) over the 1× calibration run. This is the throughput/availability trade the retune
  targets, and it is real.
- **c=100 recovers only partially toward the pre-branch (ungated) F4 baseline**, not
  substantially: Arm B c=100 is 35,448/48,588 ≈ 73% of pre-branch (vs 64% at 1×); c=1000 is
  32,645/49,475 ≈ 66% (vs 58% at 1×). Directionally correct, a genuine improvement, but "recover
  substantially toward pre-branch" oversold what 4× actually buys at c=100/1000 — reported
  honestly rather than reframed to look like a fuller recovery.
- **c=5 did not recover at all** (Arm B: 11,515 vs 11,771 at 1×, essentially flat; Arm A likewise
  10,656 vs 11,039, slightly down, within run-to-run noise). Both c=5 cells are flagged
  `generator_bound` and shed ~0% at both 1× and 4× — the admission gate was never the binding
  constraint at c=5 (only 5 requests are ever in flight against a 12-permit-or-more gate either
  way), so a wider gate has nothing to admit that wasn't already getting through. The brief's
  "c=5 ... recover[s] substantially toward pre-branch" success shape does not hold for this cell,
  and no amount of retuning `compute_admission` would change that — the ceiling here is
  closed-loop concurrency itself, not the gate. Flagging for the controller rather than chasing it
  by lowering `compute_queue`/raising the multiplier further, per the "do not retune further"
  instruction.
- Server CPU during the matrix stayed at ~330–770% (of ~1200% available on 12 cores) across every
  cell — the box was not CPU-saturated even at 4×, consistent with c=100/1000 being gate-bound
  rather than compute-bound pre-retune, and with there being some further headroom the brief did
  not ask this task to chase.

## 4. Guard cells — `cargo test -p tessera-server`

Both named guard mechanisms use explicit tiny gate configs (`ComputeGate::new(1, 0, ...)` /
`ComputeGate::new(1, 1, ...)`), not the changed defaults, so they were unaffected by the retune —
confirmed green rather than assumed:

- `state::compute_gate_tests::*` (5 tests: saturated-shed, timeout-shed, both permit-leak variants,
  status gauges) — all pass.
- `saturated_gate_sheds_a_second_viewport_with_429_and_retry_after` (D13 HTTP-level saturation
  test) and `no_permit_leak_after_a_shed_or_a_completion` (`tests/http.rs`) — both pass.

`cargo test -p tessera-server`: **29 + 35 unit/integration tests passed, 0 failed** (plus 0 doc
tests).

## 5. Full validation gates

- `cargo test --workspace` — every `test result:` line `ok`, **0 failed**, including the
  OOM-prone `write_permutation_rejects_entity_id_not_fitting_u32`-adjacent `segment_roundtrip`
  suite (ran clean, 56.87s — the known environmental flake did not reproduce this run).
- `bash scripts/check-layers.sh` — exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean, no warnings.

**Environmental incident, not a code issue**: the shared `target/` directory
(`.cargo/config.toml`'s redirect to `/home/user/code/tessera/target`, shared across this repo's
worktrees per the task brief) intermittently produced a spurious
`unresolved import tessera_authz::FragmentCacheError` compile error on `tessera-engine` — reproduced
three times across separate `cargo build`/`cargo test` invocations, always the same error, despite
the source in this worktree correctly exporting and using that symbol throughout. Diagnosed as stale
cross-worktree fingerprint/artifact reuse in the shared target dir (five sibling worktrees observed
active: `agent-a051e560bc19a14c2`, `agent-a3d07aa1988a0ad80` (`perf/b9-run-decode`),
`agent-aff6973fc9045c28e` (`probe/cell-occupancy`), `density-sampling`, plus main) — not a defect in
this change. Resolved each time with `cargo clean -p tessera-authz -p tessera-engine` followed by an
immediate rebuild; did not touch `.cargo/config.toml` per the brief. Flagging in case this recurs for
a future task in this workstream.

## 6. Files changed

- `crates/tessera-server/src/config.rs` — `COMPUTE_ADMISSION_MULTIPLIER` constant (4, with
  provenance comment), default derivation via `checked_mul`, new
  `ConfigError::ComputeAdmissionDefaultOverflow` variant + Display arm, updated field docs, updated
  test assertions/doc comments, one new overflow-refusal test.
- `crates/tessera-server/src/state.rs` — `ComputeGate`'s doc comment rewritten (requests-not-CPU
  argument).
- `crates/tessera-server/src/lib.rs` — `prepare()`'s `compute_threads` comment rewritten (no longer
  claims "one number, one meaning").
- `scripts/bench_k_sweep.py` — `write_config`'s docstring corrected to name the new default
  multiplier.
- `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/bench-runs/admission-4x/` — new run outputs
  (console log, JSON summary; gitignored SDD workspace convention, same as every prior task's
  `bench-runs/`).

## 7. Self-review

- Verified the `checked_add`/`MAX_PERMITS` bound genuinely still holds with the larger defaults by
  re-running `an_absurd_admission_plus_queue_refuses_to_start` unmodified (it operates on the final
  `compute_admission` value regardless of how it was derived) and by adding a *new*, distinct test
  for the multiplication itself overflowing (`a_compute_threads_that_overflows_the_default_admission_multiply_refuses_to_start`)
  — this is a real gap the bare `*` in a first draft of this change would have left open (silent
  wraparound in release builds, since this workspace does not enable `overflow-checks` in release),
  caught and fixed before it shipped, not merely asserted safe.
- Did not retune `COMPUTE_ADMISSION_MULTIPLIER` after seeing c=5 fail to "recover substantially" —
  per the brief's explicit instruction, reported the shortfall honestly in §3 instead of adjusting
  the constant or the queue multiplier to make the number look better.
- Re-checked every "one CPU-bound request per core" hit across the whole repo (not just the four
  files the brief named) via `grep -rn` before editing, to catch the `lib.rs` "one number, one
  meaning" comment and the `bench_k_sweep.py` docstring, both of which the brief's explicit file
  list would have missed.
- Confirmed the two guard-cell mechanisms actually use explicit tiny configs rather than assuming
  it from the brief's own claim — read `ComputeGate::new(1, 0, ...)` / `(1, 1, ...)` call sites
  directly in `state.rs` and `tests/http.rs`.
- Did not paper over the shared-target-dir compile failures — reported what they looked like, the
  diagnosis, and the fix, rather than silently retrying until green with no record.

## 8. Concerns for the controller

- **c=5 does not recover toward the pre-branch baseline under this retune, and structurally
  cannot** — it is closed-loop-concurrency-bound, not gate-bound, at both 1× and 4×. If closing
  that gap matters, the lever is elsewhere (e.g. the generator's own ceiling, or accepting that a
  5-way closed loop against a ~0.3 ms request is inherently rps-capped near `c / latency` regardless
  of gate width).
- **c=100/c=1000 recovery is real but partial** (~64%→73% and ~58%→66% of pre-branch rps
  respectively) — 4× closes roughly a third of the remaining gap to pre-branch, not "substantially"
  in a stronger sense. Server CPU (~65% of 1200% available) suggests some further headroom exists,
  but per the brief this task does not chase it further.
- **Arm A c=1000 shed% (35.2%) is still high in absolute terms**, even though it is a large
  improvement over the 1× run's 60.0% — a caller driving 1000 distinct-principal connections at this
  corpus scale will still see over a third of requests shed under sustained load.
- Shared-target-dir build flakiness (§5) is environmental, not this change, but is worth a
  standing note for whoever runs the next task in this worktree.

## Provenance correction (controller, post-review)
The review flagged the "pre-branch F4" B c=5 baseline (15,277 rps) as unsourced: it comes from the PRE-BRANCH `load.rs` module doc at the merge base — `git show 6052af2:crates/tessera-bench/src/arms/load.rs` lines 40-46 carried a c=5 row (15,277 / 0.25 ms / 1.09 ms / 315%) that Task 9's headline re-measurement later trimmed from the current file. Verified against that commit directly; the current `docs/evidence/memos/` F4 memo never had the row.
