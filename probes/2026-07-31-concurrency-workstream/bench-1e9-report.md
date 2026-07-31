# 1e9-scale bench validation — concurrency/viewpath branch, merged with main

2026-07-31, 12-core WSL2 box (shared with other active agent worktrees). Branch
`concurrency/viewpath` merged with `main` at commit `3862a61` (parents `1877546` +
`809c1b1`), which brought in main's B9 three-tier adaptive selection decode and the
"10^9 in 8 minutes" build-pipeline rewrite (`8c671f1`). **These numbers measure the
merged code — concurrency-branch work plus main's B9 decode/build-pipeline changes
together — deliberately, since that is what would actually ship.**

`categories-subclass` label set (47,968-term vocabulary), `entity_id < 1e9`,
`w=10 k=30 zoom=8`, 10 s cells, defaults otherwise (shipped 4× compute-admission).
Bundle: `/home/joe/code/tessera/data/bench-fixtures/1e9/`. Raw run output under
`bench-runs/1e9/{matrix-b,matrix-a,cpu,coldbuild}/`.

**Caveats up front**: shared box (another worktree's cargo test process was consuming
20–28 GiB RSS for part of the build window); every cell below is a single run, not a
repeated/averaged sample; disk on `/` is down to ~7 GiB free after the bundle write
(was ~54 GiB before) — tight for any further work on this box until something is
cleaned up.

---

## 1. Build: wall time and peak RSS vs the 2h36m record

```
tessera build --points geometry.parquet --pairs categories-subclass.pairs.parquet \
  --out data/bench-fixtures/1e9 --extent 0,65536,0,65536 --slice s0 --limit 1000000000 \
  --mint-external-ids --id-key 000102030405060708090a0b0c0d0e0f --epoch 1
```

| | previous record (`probes/2026-07-30-1e9-rebuild/`) | this run (merged pipeline) | ratio |
|---|---:|---:|---:|
| wall clock | 2:36:39 (9,399 s) | **10:25.0 (625 s)** | **15.0× faster** |
| peak RSS | 27,548,124 KiB (26.3 GiB) | 27,925,084 KiB (26.6 GiB) | ~unchanged (1.01×) |
| user+sys CPU time | — | 1,214.7 + 246.8 = 1,461.5 s | 233% mean CPU |
| bundle bytes on disk | 51,142,054,146 (47.6 GiB, old `/tmp/tessera-1e9`) | 47,017,049,354 (43.8 GiB) | 8% smaller |

Result: `1,000,000,000 items, 47,968 terms, 1,718,472,823 pairs`. The main-branch
rewrite (`8c671f1`, "remove the streaming pipeline's pointer-chase costs") delivers
almost exactly what its commit message advertised — **the wall clock is a ~15×
win**, not the literal "8 minutes" quoted in the coordinator's brief (10m25s vs 8m),
plausibly explained by the ~5 minutes of the run that overlapped a different
worktree's 20+ GiB cargo-test process competing for CPU/memory bandwidth on this
shared box (confirmed via `ps`/`free` during the run) — this was **not** a clean,
uncontended measurement. Peak RSS is essentially identical to the old pipeline
(makes sense: same corpus, same final in-memory structures, only the construction
algorithm changed). Bundle size dropped ~8%, consistent with the parallel-digest
changes bundled into the same main-branch merge.

## 2. Bench matrix — Arm B (c=5,100,1000), Arm A (c=100 only)

```
scripts/bench_concurrency.py --bundle .../bench-fixtures/1e9 --scale 1000000000 \
  --label-set categories-subclass --criteria matrix --w 10 --k 30 --zoom 8 --duration 10 \
  --concurrency 5,100,1000 --arms B      # matrix-b/
  --concurrency 100        --arms A      # matrix-a/  (Arm A c=1000 skipped: memory-infeasible per brief)
```

| arm | c | rps | p50 | p99 | srv p99 | cpu% | shed% | rss peak | pts/s | pts/user/s | pts/req |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| B | 5 | 4,570.8 | 0.97 ms | 2.97 ms | 2.74 ms | 538% | 0.02% | 12.1 GiB | 487,206 | 97,441.3 | 106.6 |
| B | 100 | 20,115.6 | 4.27 ms | 16.87 ms | 13.65 ms | 920% | 0.08% | 12.2 GiB | 1,677,231 | 16,772.3 | 83.4 |
| B | 1000 | 16,597.6 | 34.14 ms | 103.28 ms | 16.07 ms | 854% | 43.1% | 12.2 GiB | 1,386,871 | 1,386.9 | 83.6 |
| A | 100 | 16,820.1 | 4.75 ms | 25.00 ms | 20.88 ms | 908% | 0.0% | 19.2 GiB | 2,512,728 | 25,127.3 | 149.4 |

All RSS peaks well under the 30 GiB abort guard (Arm A c=100's 19.2 GiB is the
highest, driven by 100 distinct fragments/row-projections rather than shared state).
Note the wide client-p99/server-p99 gap at Arm B c=1000 (103.28 ms vs 16.07 ms) —
with a 43% shed rate, most of the client-side tail is queueing/429-retry overhead
in front of the gate, not server compute; the server itself stays fast even under
that much admitted load.

**Comparison against 2.42M** (`calibration-report.md` §6 / `admission-4x-report.md`
§3, same `w=10 k=30 zoom=8`, same 4× admission default):

| arm | c | rps @2.42M | rps @1e9 | srv p99 @2.42M | srv p99 @1e9 | shed% @2.42M | shed% @1e9 |
|---|---:|---:|---:|---:|---:|---:|---:|
| B | 5 | 11,515 | 4,571 | 1.33 ms | 2.74 ms | 0.01% | 0.02% |
| B | 100 | 35,448 | 20,116 | 7.12 ms | 13.65 ms | 0.08% | 0.08% |
| B | 1000 | 32,645 | 16,598 | 82.83 ms* | 16.07 ms | 1.88% | 43.1% |
| A | 100 | 32,644 | 16,820 | 7.93 ms | 20.88 ms | 0.0% | 0.0% |

(*2.42M's Arm B c=1000 p99 column in admission-4x-report.md is the **client** p99;
its server-side figure isn't broken out there the same way — compared loosely.)

Throughput roughly **halves** end-to-end going from 2.42M to 1e9 rows at matched
concurrency (expected: larger corpus → larger per-request masks/row-projections
even at the same `w`), and Arm B's shed rate at c=1000 jumps sharply (1.9% → 43%)
— the gate is absorbing real extra per-request cost at scale, shedding more rather
than degrading server latency (server p99 at c=1000 is actually *lower* than the
c=100 point, consistent with the gate doing its job under overload). This is a
single run at each scale, not a repeated sample — read the direction, not the
precise ratios.

## 3. Thread-scaling / cpu criterion cell — the headline result

```
scripts/bench_concurrency.py --criteria cpu --w 10 --k 30 --zoom 8 --duration 10 \
  --bundle .../bench-fixtures/1e9 --scale 1000000000 --label-set categories-subclass
```

| | small (z8) p50 | small pts/s | large (z4) p50 | large pts/s |
|---|---:|---:|---:|---:|
| threads=1 | 0.605 ms | 165,581 | 0.527 ms | 247,174 |
| threads=default | 1.523 ms | 70,800 | 1.154 ms | 119,072 |
| **ratio (default/1)** | **2.52×** | | **2.19×** | |

**Verdict: the calibration does NOT hold at 1e9.** At 2.42M
(`calibration-report.md` §5), the calibrated serial-fallback threshold
(`SERIAL_FALLBACK_MAX_ROWS=200,000`) delivered a small-viewport ratio of 1.01×
(target: within ~10%, PASS) and a large-viewport ratio of 0.68× (parallel
correctly *winning*). At 1e9, **both** viewport shapes regress: the small/sparse
case is 2.5× slower under `threads=default` (target was ≤1.10×, badly missed) and
the large-work case — the one shape the design explicitly exists to win — is now
2.2× *slower* under the parallel path instead of winning. Zoom-8 rows-in-ranges
at 1e9 should be roughly 1000× the 2.42M figure per the brief's own prediction,
which would push both shapes well past `SERIAL_FALLBACK_MAX_ROWS` into the
parallel branch — consistent with what's observed (parallel engages) but the
overhead/payoff balance calibrated at 2.42M evidently does not transfer: parallel
fan-out is paying a bigger tax, or the per-tile real work is proportionally
smaller, at this corpus size than the 2.42M sweep predicted. This report does not
root-cause it (would need `--features bench-timing` stage attribution at 1e9,
out of scope here) — it is flagged as the clearest actionable finding of this run.

Bonus context, criterion 4a (CPU saturation, open-loop, Arm B): target rate 18,000
rps (this boot ran `cpu` alone, without a preceding `matrix` cell in the same
process, so the target used the script's *fallback* default rather than a measured
Arm B ceiling — treat this sub-result as indicative only), achieved 15,661 rps at
850% CPU against an 680%-of-8-available-cores threshold — the server is clearly
saturating available compute under load.

## 4. Cold-build cell

```
scripts/bench_concurrency.py --criteria coldbuild --concurrency 100 \
  --coldbuild-admission-timeout-ms 250 --coldbuild-cold-workers 50 --coldbuild-warm-pool 4 \
  --w 10 --k 30 --zoom 8 --duration 10
```

cold_w=47,968 (every descriptor — the maximal-cost grant), 50 cold workers hammering
the same key, 16 warm workers on other keys:

| | requests | ok | shed | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| cold key | 351,904 | **0** | 351,903 | — | max_wall 39.36 ms |
| warm keys | 61,535 | 61,535 | 0 | 1.88 ms | 6.11 ms |

`requests_hung: 1` (out of ~413k total), `retry_after_violations: 0`.

**Shape**: at 1e9 with a near-full-corpus grant, the row-projection build the D-G
single-flight is guarding genuinely does not complete inside the 10 s window —
**zero** cold-key requests succeeded. But `cold_max_wall_ms` is only 39.36 ms,
confirming these are fast, immediate 429 sheds while the build is in flight, not
hangs — the non-blocking design holds under real 1e9-scale build cost. Warm-key
traffic on unrelated fragments is completely unaffected (p99 6.11 ms, in line with
the matrix's low-concurrency numbers). This is qualitatively the "real shape" the
brief predicted, but the cell as configured (10 s) never observes the cold build
actually *finish* — it only demonstrates sustained shedding. A longer duration (or
a dedicated single-shot timing of the cold build alone) would be needed to measure
the actual build wall-clock at 1e9.

## 5. Comparison against the pre-branch 1e9 k-sweep baseline

`docs/superpowers/plans/bench-baselines/2026-07-29-1e9-k-sweep.json`: server
p50/p99 = **5.30 / 52.71 ms at k=50** (old `/tmp/tessera-1e9` bundle, pre-branch,
`n_viewports=500`, points_returned mean 11,850 — a very different request shape
from this report's default z8 viewport, and a different sampling methodology —
sequential single-in-flight timing, not closed-loop concurrent load). This report's
closest analogue is the thread-scaling cell's `threads=default` small-viewport
server p50 of **1.351 ms** (k=30, ~106 points/request) — much lower, but k, points
returned, and concurrency regime all differ enough that this is a directional
read, not a controlled comparison: **do not** read "1.351 ms vs 5.30 ms" as a
clean 3.9× win without accounting for k=30 vs k=50 and the ~112× difference in
points returned per request.

## 6. Concerns

- **Disk**: `/` went from ~54 GiB free to ~7 GiB free after the bundle write.
  Leave headroom before running anything else disk-heavy on this box.
- **Shared-box contention**: a different worktree's cargo test process held
  20–28 GiB RSS for roughly the first third of the build; build wall time above
  is not a clean uncontended number.
- **Cross-worktree cargo cache hazard**: mid-task, `cargo build --release -p
  tessera-cli` (alone) reproducibly failed with `unresolved import
  tessera_authz::FragmentCacheError` — a symbol that plainly exists and is
  exported unconditionally (checked: not behind any `cfg`/feature gate).
  `cargo build --release -p tessera-cli -p tessera-bench` (both targets together)
  succeeded on identical source. Root-caused to a stale/corrupted cached
  `tessera_authz` artifact in the shared `target/` dir (used concurrently by
  several worktrees per `.cargo/config.toml`); `cargo clean --release -p
  tessera-authz -p tessera-engine -p tessera-cli` followed by a fresh build fixed
  it permanently for this session. Not a source bug — flagged because
  `reference/oracle/harness.py`'s `ensure_cli_built()` calls the bare `-p
  tessera-cli` form on every bench boot, so this can silently recur on this
  shared box.
- **Single run, single box**: none of the cells above are repeated or averaged;
  read magnitudes and directions, not precise ratios.
- All numbers here reflect the **merged** code (concurrency/viewpath + main's B9
  decode and build pipeline) — deliberate, per the brief, since that is what
  would ship.
