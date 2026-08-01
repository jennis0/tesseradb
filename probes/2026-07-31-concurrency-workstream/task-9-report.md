# Task 9 report — bench validation gate

2.42M `categories-subclass` fixture (176-term vocabulary), 12-core WSL2 box, generator co-resident
(4 of 12 cores), w=10, k=30, zoom=8, 10 s per cell — matching the F4 memo's own conditions except
duration (brief specifies 10 s; the memo said 8 s). All numbers below are from real runs against
this branch's code (commit range up to and including `cfa6e83`, Tasks 1-8), captured
2026-07-30/31.

## Summary verdict

| # | Criterion | Verdict |
|---|---|---|
| 1 | F4 gone | **PARTIAL FAIL** — lock-contention signature genuinely gone (p99 1042ms→59ms); raw rps target (≥35k) missed (10.1k measured), for a structural reason, not a residual bug |
| 2 | No hangs at c≫cores | **PASS** — 0 hung requests across every cell; every request terminated 200 or 429 |
| 3 | Shed activates at the bound | **PASS** |
| 4 | CPU saturation | **FAIL** — both sub-parts measured below bar (79.99% of available cores vs 85% target; c=1 threads=default 2.97x *slower* than threads=1, not faster) |
| 5 | Pan-storm bounded | **PASS** |
| 6 | Cold-build storm graceful | **PASS on the load-bearing claims**, with a documented confound in the warm-traffic shed rate |
| 7 | Byte-identity | **PASS** |

Failures are reported as measured; nothing below was re-run or reconfigured to change a number
after seeing it. Where a failure has a structural explanation (the admission gate changing what
"throughput" even means), that explanation is given for the adjudicator, not as a substitute for
the number.

---

## Criterion 1 — F4 gone

**Threshold:** Arm A c=1000: rps 18.6k → ≥35k (excluding `generator_bound` cells); e2e p99
1,042 ms → <100 ms; server CPU must rise with concurrency, never fall.

### Full matrix (this run)

| arm | conc | rps | p50 | p99 | server p99 | server CPU | shed% |
|---|---:|---:|---:|---:|---:|---:|---:|
| B shared | 5 | 1,728.2 | 0.73 ms | 28.25 ms | 27.85 ms | 68.8% | 0.86% |
| B shared | 10 | 2,037.9 | 1.10 ms | 53.20 ms | 52.84 ms | 75.2% | 1.19% |
| B shared | 100 | 9,072.6 | 3.35 ms | 40.84 ms | 14.79 ms | 423.8% | 81.66% |
| B shared | 1000 | 13,695.6 | 15.50 ms | 51.90 ms | 5.51 ms | 663.1% | 79.17% |
| A distinct | 5 | 6,437.5 | 0.69 ms | 2.32 ms | 2.09 ms | 537.5% | 0.0% |
| A distinct | 10 | 10,798.0 | 0.74 ms | 3.33 ms | 3.05 ms | 646.3% | 0.0% |
| A distinct | 100 | 12,989.9 | 3.42 ms | 10.17 ms | 6.03 ms | 628.6% | 79.63% |
| A distinct | 1000 | **10,081.3** | 17.50 ms | **59.24 ms** | 8.61 ms | **609.7%** | 83.77% |

Generator ceiling (healthz): 26.3k/85.7k/93.4k/74.8k rps at c=5/10/100/1000 (falls at c=1000 —
generator saturating there itself, as in the original memo). No cell above is flagged
`generator_bound` (all viewport rps figures are well under 1/3 of the matching healthz ceiling).

### Against the memo

| | F4 memo (pre-fix) | This run (post-fix) |
|---|---:|---:|
| Arm A c=1000 rps | 18,599 | **10,081.3** |
| Arm A c=1000 e2e p99 | **1,042.31 ms** | **59.24 ms** |
| Arm A c=1000 server p99 | 10.49 ms | 8.61 ms |
| Arm A c=1000 CPU | **426%** | **609.7%** |
| Arm A c=100 → c=1000 CPU delta | 712% → 426% (**collapse**, −40%) | 628.6% → 609.7% (mild, −3%) |

**e2e p99 — PASS, decisively.** 1,042 ms → 59.24 ms (94% reduction), comfortably under the
<100 ms bar.

**CPU signature — mostly reversed, not strictly monotonic.** F4's smoking gun was CPU
*collapsing* while the system got slower (712%→426%, a session-churn lock held across the whole
`RowProjection::new` build). That collapse is gone: Arm A's CPU across c=5/10/100/1000 is
537.5%→646.3%→628.6%→609.7% — it rises sharply into c=10, then plateaus/drifts down slightly at
c=100/1000. The literal "never fall" wording is not strictly satisfied (a ~6% relative dip from
the c=10 peak), but the mechanism is entirely different from F4's: at c≥100, 80-84% of Arm A's
requests are gate-shed (429, near-zero CPU cost each), so the *mix* of work shifts toward cheap
rejections as concurrency rises past `compute_admission`+`compute_queue` (36 slots) — a
by-design consequence of Task 4's gate, not lock contention. A CPU-bound system whose *admitted*
work collapses under a lock (F4) is categorically different from one whose *rejection rate* rises
(this run). Read as: the F4 signature is gone; the literal monotonicity clause is not perfectly
met and is flagged for the adjudicator rather than asserted as a clean pass.

**rps — FAIL against the literal ≥35k bar, structurally caused by Task 4's gate.** 10,081 rps is
*below* even the pre-fix 18,599 rps, and this is the honest number. The reason is not F4
recurring: `ComputeGate` (D-B, landed after the F4 baseline was recorded) caps *running* compute
at `compute_admission` (defaults to available cores — 12 here) with a bounded queue
(`compute_queue`, default 24) on top; anything beyond 36 admitted+queued requests sheds
immediately (429). At c=1000 this means the vast majority of arrivals (83.77%) are shed by
design, and successful-request throughput is ceilinged at roughly
`compute_admission / per_request_service_time` — a few thousand to ~14k rps at this fixture's
per-request cost, independent of how fast the underlying compute actually is. The ≥35k figure
predates the gate's existence (it was set against an *ungated* server, where 1000 concurrent
requests were all genuinely attempted, just serialised behind one mutex). Measuring it against
the now-gated server without raising `compute_admission`/`compute_queue` compares two different
systems. This is offered as context, not as a substitute for the result: **as literally worded,
the criterion fails**, and whether the ≥35k figure should be re-derived against the gate's own
sizing is the controller's call, not mine to retune here.

### Anomaly: c=5/10 shed 150/245 requests (Arm B) — checked, not guessed

Arm B shares only 4 tokens regardless of `c` (`pool_size = min(4, c)`). At c=5/10, concurrency is
far below the gate's 36-slot capacity, so **the D-B admission gate cannot trip** — confirmed
empirically, not assumed: an isolated repro (same fixture, same params, `c=5`, Arm B) sampled
`/control/status`'s `compute.shed_total` (Task 4's own gate-only counter — its doc note is
explicit that Building-429s are *not* included in it) before and after the load cell:

```
status_before: {'admission': 12, 'in_flight': 0, 'queue': 24, 'shed_total': 0, 'waiting': 0}
status_after:  {'admission': 12, 'in_flight': 0, 'queue': 24, 'shed_total': 0, 'waiting': 0}
gate shed_total delta: 0
bench-reported requests_shed_429: 10   (of 69,388 total requests in this repro)
```

`shed_total` delta is **0** while the bench client observed real 429s. Since `shed_total` counts
*only* D-B gate sheds, these 429s are D-G's single-flight building-shed
(`EngineError::ProjectionBuilding`/`FragmentBuilding`, Tasks 1-2): with only 4 shared tokens,
several workers land on the *same* token and race its first-touch row-projection build
concurrently; the non-blocking single-flight cache sheds every loser with 429 instead of queueing
them (exactly D-G's design). Arm B's `pool_size=min(4,c)` means more workers share fewer tokens as
`c` rises within this regime (c=10 packs up to 3 workers onto one token vs c=5's 2), which is why
c=10 sheds more (245) than c=5 (150) — consistent with the mechanism, not with gate exhaustion.
This is the same phenomenon the brief's own Task 1-2 context note names for Arm B's c=1000 opening
burst, just proportionally smaller at c=5/10. **Not a new defect; the D-G fix working as intended,
visible in the shed-rate column added for this task.**

---

## Criterion 2 — no hangs at c ≫ cores

**Threshold:** every request terminates 200 or 429; none exceeds `admission_timeout_ms` + 2 s.

`requests_hung` is **0** in every cell of the matrix (all 8 viewport cells, both arms, all four
concurrency levels including c=1000 = 83x the 12-core box), enforced by the hang-watchdog wired
into every matrix load call (`--hang-timeout-ms` = `admission_timeout_ms` (250) + 2000 = 2250 ms,
a hard client-side deadline distinct from `reqwest`'s 120 s connection timeout). `max_wall_ms`
peaked at 410.45 ms (Arm A c=1000) — comfortably under the 2,250 ms bound. Also 0 hung in the
shed cell (c=1000, hang timeout 5,250 ms), the pan-storm cell, and the cold-build cell. **PASS,
clean.**

---

## Criterion 3 — shed activates at the bound

**Threshold:** forced-low admission → shed rate > 0, all 429s carry `Retry-After`, served p99
stays bounded.

Boot with `compute_admission=1, compute_queue=0`, c=1000 (matching the matrix's own top
concurrency level), 10 s:

```
shed_rate:               98.43%   (842,151 of 855,592 total requests)
requests_ok:              13,441
retry_after_violations:        0
served_p50 / p99:      10.49 ms / 41.92 ms
requests_hung:                  0
```

Shed rate > 0 (in fact dominant, as expected with a 1-permit gate under 1000-way concurrency),
every one of the 842,151 429s carries both `Retry-After: 1` and body `retry_after_s: 1`
(0 violations — verified per-response by the load generator, not sampled), and the requests that
*do* get served stay fast (p99 41.92 ms) rather than degrading as admission tightens. **PASS.**

---

## Criterion 4 — CPU saturation

**Threshold:** open-loop at ≈capacity: server CPU ≥~85% of available cores (12 − generator's 4 =
8); c=1 latency improves on multi-tile viewports with tile-loop parallelism on.

### 4a — open-loop saturation

```
target_rate:       12,326 rps  (0.9x the matrix's own Arm B c=100 ceiling)
achieved:          12,321.4 rps
cpu_mean:          639.88%   =  79.99% of 8 available cores
threshold:         680.00%   =  85% of 8 available cores
```

**Measured 79.99%, below the 85% bar — FAIL, by a real but not huge margin** (~5 percentage
points of 8 cores, i.e. ~0.4 cores short). The gap is plausibly capacity-vs-demand: `rate` was
set to 90% of a *closed-loop* ceiling, and 32 workers driving that rate may simply not offer
quite enough continuously-queued demand to keep all `compute_admission=12` slots saturated every
sampled interval, rather than the server being unable to reach 85%. Reported as measured, not
adjusted upward to clear the bar.

### 4b — c=1 latency, multi-tile viewport, `compute_threads` 1 vs machine default

Two separate boots (`compute_threads=1` vs unset → 12), single worker, zoom 8 (the same ~256-tile
span the rest of this suite and `bench_k_sweep.py`'s own convention use), 5 s:

```
threads=1        c=1 p50=0.376 ms   server_p50=0.206 ms
threads=default  c=1 p50=1.118 ms   server_p50=0.936 ms
```

**threads=default is ~2.97x *slower*, not faster — FAIL, measured, not tuned.** This is the
opposite of the criterion's expectation. The most likely explanation, not independently
re-verified under this task's time budget: at this fixture's scale (2.42M rows) and this zoom's
work-per-tile, a 256-tile viewport's per-tile gather is cheap enough that `rayon`'s `pool.install`
/ work-stealing overhead for a 12-thread pool exceeds the parallel win, especially for a single
in-flight request with no other compute contending for the pool — i.e. the tile loop's own
`TILE_PAR_MIN_LEN`-style oversubscription threshold (`tessera-store::Permutation::project`'s doc
references an analogous guard) may not be tuned for this corpus size, or c=1 simply never
generates enough parallel tile work to amortise pool overhead at 2.42M. **Not investigated
further per the controller's "do not tune to pass" instruction** — reported as measured.

---

## Criterion 5 — pan-storm bounded (warm sessions)

**Threshold:** aborts every ~50 ms at c ≥ cores; completed-request throughput within ~20% of the
no-abort cell; no permit leak over the run.

c=30 (comfortably inside the 36-slot gate capacity, so the run exercises real compute-queue
waiting rather than pure slot-shedding), warm sessions (one full viewport fired per token before
the timed window), 50 ms abort deadline, 10 s each:

```
baseline_rps (no abort):  15,558.1
storm_rps (with abort):   14,384.3
ratio:                     0.9246   (within ~20%: PASS)
storm_aborted:                 87   (the abort deadline fired for real, not a no-op cell)
storm_hung:                     0
gate status before: {admission: 12, in_flight: 0, queue: 24, shed_total:   0, waiting: 0}
gate status after:  {admission: 12, in_flight: 0, queue: 24, shed_total: 700, waiting: 0}
```

Throughput ratio 0.92 (within the ~20% band), 87 requests genuinely hit the client's 50 ms abort
deadline (so this cell exercised real abandon-and-reissue traffic, not a cell where nothing ever
needed aborting), 0 hung, and `in_flight`/`waiting` are both back to 0 after the run — consistent
with no permit leak (`shed_total` rising to 700 is expected ordinary gate shedding at c=30 > 12
admission slots, unrelated to leakage). **Correction on what this cell proves:** `storm_aborted`
counts client-side abort events only; on its own it is equally consistent with the server actually
running each aborted request to completion in the background (never cancelling) as with D-C's
cancellation genuinely engaging — an HTTP-level bench cannot distinguish "the server freed the
permit promptly because it cancelled" from "the server happened to finish around the same time
anyway." The permit-accounting gauges above (`in_flight`/`waiting` back to 0) are consistent with
cancellation but not proof of it either, since ordinary completion produces the same end state. The
actual proof that D-C's cancellation mechanism fires is
`crates/tessera-server/tests/http.rs::dropping_a_client_connection_mid_viewport_releases_the_gate_promptly`,
a dedicated unit-level test that measures permit-release latency directly; this bench cell
corroborates a pattern consistent with that mechanism operating at real HTTP/OS-thread scale under
sustained load, which the unit test alone cannot exercise, but does not itself establish the
mechanism. **PASS** on the throughput-ratio and no-hang claims this criterion is actually about.

---

## Criterion 6 — cold-build storm graceful

**Threshold:** same-key arrivals shed fast (429, not multi-second waits); warm traffic on *other*
keys continues to be served; the builder completes exactly once per key.

**Approximation used, and why.** At 2.42M scale this fixture's `categories-subclass` vocabulary is
only 176 terms; even granting a session *every* descriptor (maximal buildable coverage without an
artificial sleep — explicitly ruled out by the brief) makes `RowProjection::project` cost on the
order of tens of milliseconds, not seconds. Two levers were used, both legitimate: (1) the cold
token is authorised with all 176 descriptors (`--coldbuild-cold-w`, defaulting to every
descriptor) to maximise mask cardinality/build cost; (2) `admission_timeout_ms` was lowered to
5 ms (`--coldbuild-admission-timeout-ms`, from the 250 ms default) so the measured build is
clearly **≫** the timeout, which is the property the criterion needs exercised (this is a
deployment knob, not a code change, and is exactly the kind of override this task's own deliverable
2 — `compute_*` config flags in the script — exists to make possible).

```
cold_w:                       176 descriptors (~100% coverage — deliberately maximal)
admission_timeout_ms:           5
cold_workers:                 200  (all hammering ONE shared, never-yet-queried token)
warm_workers:                  32  (round-robin over 8 separately pre-warmed tokens)

cold key:  62,691 ok / 572,009 shed (429)   max_wall = 51.52 ms
warm keys: 10,317 ok /  92,780 shed (429)   p99 = 16.77 ms
requests_hung: 0    retry_after_violations: 0
```

**Same-key sheds fast, never a multi-second wait — PASS.** `cold_max_wall_ms` is 51.52 ms
(≈10x the 5 ms timeout, comfortably "≫"), nowhere near a multi-second wait even under the
most aggressive timeout tested.

**Warm traffic is genuinely served, and never blocked behind the cold build — PASS on that
specific claim.** 10,317 successful warm-key responses landed with **p99 16.77 ms** — low,
un-degraded latency whenever a warm request *was* admitted, which is the actual claim under test
(D-G's non-blocking single-flight means no request ever waits *on another key's build*; a request
either runs on its own key's already-warm cache or it doesn't touch the cold build's state at
all).

**Caveat, reported rather than hidden: warm-key requests were ALSO shed at a high rate (90%,
92,780 of 103,097)**, and this needs to be read correctly. `compute_admission`/`compute_queue`
were left at their defaults (12/24) for this cell — only `admission_timeout_ms` was lowered — so
the shared D-B gate (not any per-key mechanism) is genuinely saturated by 232-way total
concurrency (200 cold + 32 warm workers) against 36 gate slots and a 5 ms timeout. That gate
capacity is shared across every key by design (D-B does not partition by session), so a large
total-concurrency storm sheds warm traffic too — but because the gate is full, not because warm
keys are waiting behind the cold build. This is a confound of this cell's own parameter choice
(a large `cold_workers` count plus an aggressive shared timeout, chosen to force the cold build
≫ timeout), not evidence against D-G's non-blocking claim. Not re-run with different parameters
to produce a cleaner-looking warm-shed number, per the controller's instruction.

**Builder completes exactly once per key.** Not independently re-derivable from HTTP-level bench
data alone (a 429 and a fast warm 200 are wire-indistinguishable from a client). The authoritative
proof is `crates/tessera-engine/tests/viewport.rs::concurrent_same_key_viewports_single_flight_others_get_projection_building`
and `distinct_key_first_viewports_overlap_instead_of_serialising` (Task 1) and their
`tessera-authz` fragment-cache twins (Task 2) — both green in this run's `cargo test --workspace`
(see Criterion 7 below) — which assert exactly-once-build with a counting builder under real
concurrent access. This bench cell corroborates the *observable pattern* consistent with
exactly-once (one build-cost-shaped population of successes, everything else fast-shed or
fast-warm) at real HTTP/OS-thread scale, which the unit tests cannot exercise.

**Overall: PASS on the two load-bearing claims (fast shed, no cross-key blocking); the high
aggregate warm-shed number is reported and explained, not smoothed over.**

---

## Criterion 7 — byte-identity

**Threshold:** Task 6 equality tests + oracle suite (`reference/`) unchanged.

```
cargo test -p tessera-server --test http           34 passed; 0 failed
  includes viewport_response_body_is_byte_identical_at_compute_threads_1_and_8 ... ok

cargo test -p tessera-engine --features bench-timing --test viewport   36 passed; 1 ignored; 0 failed
  includes viewport_output_is_byte_identical_at_compute_threads_1_and_8 ... ok
  includes concurrent_same_key_viewports_single_flight_others_get_projection_building ... ok
  includes distinct_key_first_viewports_overlap_instead_of_serialising ... ok

reference/.venv/bin/python -m pytest reference/tests -q     30 passed
```

No wire-byte assertion changed; the bench/script changes in this task touch only
`crates/tessera-bench` and `scripts/`, neither of which the equality tests or the oracle suite
exercise. **PASS.**

---

## Environment incidents during this task (transparency)

Two infrastructure problems occurred while running the acceptance cells; both are recorded here
because they affected timing and could otherwise look like silent gaps in the run.

1. **The first attempt at the matrix/cpu run (criteria 1/2/4) died silently** partway through Arm
   B's c=1000 cell (7 of ~15 cells completed; token files up to c=1000 present, then nothing).
   No Python traceback was captured (buffered stdout was lost, consistent with a SIGKILL rather
   than a clean error or hang). This coincided with a sibling worktree's `segment_roundtrip` test
   process holding ~21 GB RSS on this shared 47 GB box at the same timestamp; that process had
   exited by the time of the restart and the box had 44 GiB available again. The run was
   restarted once from a clean state and completed; the numbers in this report are from that
   completed run. **If a bench cell above looks anomalously slow or memory-heavy and this
   incident is relevant context, this is the place to check first** — no cell in the reported
   run showed the RSS or latency signature of memory pressure, but the box is shared and this is
   named per the controller's explicit request to factor memory contention into interpretation.
2. **The shared `target/` build cache (this box's `.cargo/config.toml`-configured, cross-worktree
   shared target dir) intermittently served a stale `tessera-authz` artifact** lacking a symbol
   (`FragmentCacheError`) that is unconditionally exported in this branch's source — reproduced
   deterministically against the shared dir, absent when built against an isolated
   `CARGO_TARGET_DIR`, and resolved by `cargo clean --release -p tessera-authz -p tessera-engine
   -p tessera-cli -p tessera-server -p tessera-build -p tessera-bench` followed by a fresh build.
   Not a source bug in this branch — a cross-worktree cache staleness issue on the shared target
   dir, worth a maintenance note but out of this task's scope to fix. Separately, disk hit 100%
   full (117 MiB free) mid-way through a full `cargo test --workspace` run in a temporary isolated
   target dir (a debug/test-profile build of the whole workspace needs more headroom than this
   box's ~8 GiB free at the time); recovered by deleting the isolated dir and switching to the
   shared dir, which reused its already-built dependency artifacts. `write_permutation_rejects_entity_id_not_fitting_u32`,
   the pre-existing OOM-prone test the task instructions flagged, ran and passed cleanly (78.57 s)
   in the successful `cargo test --workspace` run.

---

## Deliverables

1. **`crates/tessera-bench/src/arms/load.rs`** — per-status accounting (200/429/other/network-error
   distinguished; shed vs error vs hang vs abort are no longer conflated), `Retry-After`
   header+body verification per 429, a hang-watchdog (`--hang-timeout-ms`, a hard client deadline
   distinct from the 120 s connection timeout), pan-storm mode (`--pan-storm`,
   `--abort-after-ms` — races each request against a deadline and drops it, D-C-style, on loss),
   and the cold-build split (`--cold-workers` — first N workers hammer `tokens[0]`, the rest
   round-robin `tokens[1..]`). `StormOptions` groups the four new knobs. New `RawOutcome`/`issue`/
   `from_result` helpers factor the status/header/body extraction out of `drive`'s three call
   shapes (plain/pan-storm/watchdog) into one place. Module doc updated with the Task 9
   re-measurement (headline only; full numbers are this report).
2. **`crates/tessera-bench/src/main.rs`** — CLI flags for the four `StormOptions` fields on the
   `load` subcommand.
3. **`scripts/bench_concurrency.py`** — `--compute-threads`/`--compute-admission`/
   `--compute-queue`/`--admission-timeout-ms` (threaded to the main matrix boot), a `shed_rate`
   /`requests_hung` column in the printed table and JSON summary, and four new functions:
   `run_cpu_saturation_cell` (criterion 4, both sub-parts), `run_shed_cell` (criterion 3),
   `run_panstorm_cell` (criterion 5), `run_coldbuild_cell` (criterion 6) — each boots its own
   server (compute-gate config is boot-time). `--criteria` selects which run (default: all five).
4. **`scripts/bench_k_sweep.py`** — `write_config_with_max_k`/`spawn_with_long_boot_deadline`
   gained optional, keyword-only `compute_threads`/`compute_admission`/`compute_queue`/
   `admission_timeout_ms` overrides (`None` default = unchanged behaviour; every other caller of
   these two functions — `bench_p99.py`'s own local copy is untouched, and `bench_work_correlation.py`
   /`bench_fixed_viewport.py`/`measure_sampler_tie_threshold.py` all call positionally and are
   unaffected).

None of deliverables 1-2 change wire bytes (confirmed by criterion 7). Run outputs
(`concurrency-summary.json`, `load.jsonl`, token files) are transcribed into this report and were
not committed, per the brief's guidance and `docs/archive/plans/bench-baselines/`'s convention
being specific to `cargo bench` regression-gate baselines, a different harness path this task did
not touch.

## Self-review

- Re-read `load.rs`'s new code end to end after formatting; the `Sample`/`RawOutcome` split keeps
  the three call shapes (plain, pan-storm-wrapped, watchdog-wrapped) sharing one status/body
  extraction path rather than tripling it.
- Verified the `wall`/`server` `Timing::from_samples` calls are now guarded against an empty
  `ok` set (a latent pre-existing panic risk the forced-low-admission shed cell would otherwise
  have hit first, since a real deployment could plausibly see zero successes in a short window).
- Confirmed via a direct `/control/status` probe (not inference) which mechanism explains the
  c=5/10 shed anomaly, per the controller's explicit instruction not to guess.
- Confirmed `cargo fmt -p tessera-bench -- --check` clean, `cargo clippy -p tessera-bench
  --all-targets` clean, `cargo test --workspace` all green (including the OOM-prone
  `write_permutation_rejects_entity_id_not_fitting_u32`, no SIGKILL this run), `bash
  scripts/check-layers.sh` exit 0, and `reference/tests` (30/30) after the environment incidents
  above were resolved.
- Did not touch `.cargo/config.toml`, wire formats, or any file outside
  `crates/tessera-bench`/`scripts/`.
- Did not re-tune any configuration after seeing a failing number; the two documented
  environment-incident restarts were re-runs of a *died* process from a clean starting state
  (identical parameters), not parameter changes made in response to a result.

## Concerns for the controller

- Criteria 1 (rps) and 4 (both sub-parts) fail as literally worded. Criterion 1's rps bar
  predates Task 4's admission gate and may need to be re-derived against gate-bounded throughput
  rather than ungated throughput — that is a criterion-design question, not something this task
  should resolve unilaterally. Criterion 4b's parallel-slower-than-serial result at c=1 was not
  investigated beyond a plausible hypothesis (oversubscription overhead at this corpus's
  per-tile-work scale); if it matters, it needs its own follow-up, ideally with `bench-timing`
  stage attribution to see whether the extra time is inside `pool.install` or elsewhere.
- The cold-build cell's warm-key shed rate (90%) is a real number with a real, stated confound
  (shared gate capacity, not cross-key blocking); a cleaner isolation would lower
  `cold_workers`/raise `compute_admission` for that cell specifically, which this task did not do
  once real numbers existed, per instruction.
- Two shared-infrastructure issues (cross-worktree target-dir cache staleness; disk filling to
  100% under a debug/test-profile build with only ~8 GiB free) are worth a maintenance note beyond
  this task — neither is a defect in this branch's code, but both cost real time here and would
  recur for any concurrent task on this box.
