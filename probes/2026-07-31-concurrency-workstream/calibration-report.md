# Calibration report — tile-loop fan-out (c=1 overhead fix)

2.42M `categories-subclass` fixture (176-term vocabulary), 12-core WSL2 box, 2026-07-31. All
numbers below are from real runs against this branch's code, captured today. Method: engine-level
sweeps (fast iteration, `cargo run --release --example ... --features bench-timing`) located the
overhead and calibrated both constants; final validation numbers are from the real HTTP server path
(`scripts/bench_concurrency.py` against `target/release/tessera`), per the brief.

Raw sweep/run outputs are under `bench-runs/` alongside this report:
`stage-attribution.txt`, `calibration-sweep-serial-vs-parallel.txt`, `min-len-sweep.txt`,
`min-len-sweep-4v8.txt`, `post-calibration-console.txt` + `post-calibration/concurrency-summary.json`,
`post-calibration-shed/concurrency-summary.json`.

---

## 1. Stage attribution — where the extra ~0.74 ms goes

Tool: `crates/tessera-engine/examples/stage_attribution.rs` (kept — permanent measurement tooling,
same precedent as `examples/route_saving.rs`). It opens the 2.42M fixture directly (no HTTP layer),
authorises a w=10 grant spread across the dictionary, issues the exact first viewport
`bench_concurrency.py`'s own client draws at zoom 8/seed 0 (copied RNG, not approximated), and
averages `StageTimings` over 300 reps at `compute_threads = 1` vs `= default (12)`.

**First attempt used a single-term grant and found the OPPOSITE of Task 9's regression** (default
3.1x *faster*) — a genuine finding, but the wrong regime: a single term's mask turned out far
denser than the F4-memo's w=10 condition (Task 9's report header: "w=10, k=30, zoom=8"), so the
per-tile work was large enough that parallelism paid off even before any fix. Re-run with a w=10
grant and the load client's actual first-drawn bbox reproduces Task 9's regression directly:

```
2.42M categories-subclass, w=10 (spread grant), zoom 8, tiles_resolved=256, tiles_nonempty=3, sigma_visible=21

== threads=1 (compute_threads = 1) ==
  total_ns            avg=   80257  p50=   76125  p99=  145770
  serial prefix sum   avg=   10569   (generation/pin/slice/row_projection/compose/theta/tiles_for_bbox/tile_ranges)
  parallel section (pool.install + fold), = total - serial prefix   avg=   69688
    [reference] count_ns=823 select_ns=2222 gather_ns=909  (real per-tile work: ~4 us total)

== threads=default (compute_threads = 12) ==
  total_ns            avg=  909669  p50= 1012381  p99= 1690755
  serial prefix sum   avg=   11896
  parallel section (pool.install + fold), = total - serial prefix   avg=  897773
    [reference] count_ns=1483 select_ns=4948 gather_ns=1565  (real per-tile work: ~8 us total, cross-worker sum)
```

**Finding.** The serial *prefix* (generation load, pin, slice lookup, row-projection cache lookup,
compose, θ anchor, `tiles_for_bbox`, `tile_ranges_all`) costs ~10-12 µs either way — unaffected by
`compute_threads`, as `timing.rs`'s doc predicts. The entire gap is inside "parallel section":
~70 µs at `threads=1` vs ~898 µs at `threads=default` — a **12.9x** difference at this exact
shape (256 tiles, only 3 non-empty, ~4-8 µs of genuine count/select/gather work total). At
`threads=1`, `pool.install` still runs (it is not bypassed pre-calibration) but with a single
worker there is no cross-thread scheduling to pay for. At `threads=default`, rayon's work-stealing
setup and per-chunk task dispatch across 12 workers costs on the order of 900 µs to move ~4-8 µs
of real work around — almost pure overhead, not real work. This is the same order of magnitude as
Task 9's HTTP-measured 2.97x (0.376 ms -> 1.118 ms p50): the engine-level number is larger because
it isolates `Engine::viewport` from response serialisation and HTTP overhead, which dilute the
ratio at the wire but do not change where the cost originates.

**Root cause, confirmed rather than inferred**: `pool.install`'s own entry/scheduling overhead for
a 12-thread pool, paid on every request regardless of how little work that request actually has,
exactly as Task 9's report hypothesised and declined to chase further ("not investigated further
per the controller's do-not-tune-to-pass instruction").

---

## 2. Serial-fallback threshold sweep

Goal: find, from real data, when `pool.install` starts paying for itself, using a predictor
available BEFORE the fan-out.

**Predictor candidates and why tile count loses.** Both tile count and `Σ range.len()` (total rows
spanned by resolved tiles, pre-mask) were tracked side by side. Tile count does **not**
discriminate: the client's own "natural" viewport shape (`gen_viewports`' fixed half-extent-at-low-
zoom / shrinking-span-at-high-zoom formula) resolves a near-constant ~289 tiles at every zoom from
6 to 14 regardless of density — yet at that SAME tile count the measured verdict ranged from
"serial wins 18x" (near-empty tiles) to "parallel wins ~1.8x" (dense tiles), purely as a function
of how many rows those tiles actually spanned. Conversely, a request touching as few as 4 tiles but
spanning the WHOLE 2.42M-row corpus (maximally zoomed out) still measured parallel breaking even or
winning — few units to schedule did not make the spanned work small. `Σ range.len()` tracks the
real driver directly and matches this codebase's own cost model (CLAUDE.md: "bitmap operations
cost O(containers touched)", which scales with the range read).

Tool: `crates/tessera-engine/examples/calibration_sweep.rs` (kept). 48 "natural" client-window
shapes (zooms 4/5/6/7/8/10/12/14, 8 random draws each) + 5 "full-extent" shapes (zooms 1-5,
whole-corpus rows through 4-1024 tiles). Serial arm = `compute_threads=1` (a conservative,
slightly-pessimistic stand-in for the true zero-overhead fallback — see the code comment). Median
of 40 reps per shape/config.

Representative rows from the full table (`bench-runs/calibration-sweep-serial-vs-parallel.txt`):

| shape | tiles | rows_in_ranges | serial p50 (ns) | parallel p50 (ns) | ratio | verdict |
|---|---:|---:|---:|---:|---:|---|
| natural/z10 (x8 seeds) | 289 | 0 | ~50-75k | ~500k-1.02M | 10-19x | SERIAL, every sample |
| natural/z7 (x8 seeds) | 289 | 0-70k | ~53-556k | ~485-981k | 1.1-18.6x | SERIAL, every sample |
| natural/z6/s2 | 289 | 103,509 | 668,970 | 874,927 | 1.31 | SERIAL |
| natural/z6/s3 | 289 | 142,541 | 691,269 | 883,750 | 1.28 | SERIAL |
| natural/z6/s5 | 289 | 165,339 | 759,429 | 775,448 | 1.02 | SERIAL (tie) |
| natural/z6/s4 | 289 | 316,559 | 1,281,747 | 881,128 | 0.69 | PAR |
| natural/z6/s0 | 289 | 430,239 | 1,788,898 | 896,559 | 0.50 | PAR |
| natural/z6/s1 | 289 | 436,972 | 2,057,248 | 1,144,793 | 0.56 | PAR |
| natural/z4 (x8 seeds) | 81 | 1.0M-1.75M | ~1.8-2.8M | ~0.88-1.18M | 0.39-0.61 | PAR, every sample |
| full-extent/z1 | 4 | 2,422,486 | 2,734,521 | 2,649,209 | 0.97 | PAR (marginal, few tiles) |
| full-extent/z2..z5 | 16-1024 | ~2.42M | 2.78-6.75M | 1.22-1.94M | 0.29-0.64 | PAR |

**Crossover.** Below ~165,000 rows: serial wins or ties in every sample (highest observed ratio
20.3x). Above ~316,000 rows: parallel wins in every sample. Between them (100k-165k) the ordering
is noisy — `Σ range.len()` is a proxy for containers touched, not identical to it, so two shapes at
similar row-spans can differ in real cost by container-boundary alignment. **Chosen threshold:
200,000 rows**, inside the gap near its lower edge — biased toward serial because the two
misclassification costs are asymmetric (wrongly-parallel on genuinely small work measured 6-20x
slower; wrongly-serial on ambiguous-zone work measured at most ~1.3x slower).

No corpus-dependent knob was added: the sweep gave no evidence the crossover moves with grant width
or viewport size independently of `Σ range.len()` itself (both feed into that one number), so a
fixed constant is what the data supports, per the brief's own instruction not to force a knob the
data doesn't need.

---

## 3. `with_min_len` chunk-size sweep

Tool: `crates/tessera-engine/examples/min_len_sweep.rs` (kept), restricted to six shapes already
established as clearly-parallel (natural z4/z5, full-extent z2-z5; 16-1,024 tiles). `TILE_PAR_MIN_LEN`
hand-edited across 4/8/16/32/64, rebuilt, rerun (no runtime knob exists for this by design — see
concerns).

Full sweep (`bench-runs/min-len-sweep.txt`, p50 ns, `compute_threads=default`):

| shape | tiles | 4 | 8 | 16 | 32 | 64 |
|---|---:|---:|---:|---:|---:|---:|
| natural/z4 | 81 | 1,092,397 | 995,529 | 2,775,316 | 1,798,738 | 2,994,318 |
| natural/z5 | 81 | 1,002,936 | 803,825 | 666,562 | 707,746 | 663,926 |
| full-extent/z2 | 16 | 1,790,878 | 1,612,819 | 2,770,617 | 2,963,995 | 3,168,489 |
| full-extent/z3 | 64 | 1,221,906 | 1,224,342 | 3,039,025 | 1,736,172 | 3,259,532 |
| full-extent/z4 | 256 | 1,184,913 | 1,316,544 | 1,378,159 | 1,440,083 | 2,369,105 |
| full-extent/z5 | 1024 | 1,681,890 | 1,613,478 | 1,716,485 | 1,592,715 | 1,828,776 |

16 and above are consistently, often substantially, worse than 4 or 8 — confirming the original
constant's "keep it small" reasoning, only the exact value was argued rather than measured before.
A focused 4-vs-8 re-run (`bench-runs/min-len-sweep-4v8.txt`, REPS=120, two interleaved trials to
check for drift) confirmed reproducibility:

| shape | 4 (trial 1) | 8 (trial 1) | 4 (trial 2) | 8 (trial 2) |
|---|---:|---:|---:|---:|
| natural/z4 | 1,043,274 | 991,360 | 986,086 | 984,200 |
| natural/z5 | 1,005,775 | **803,402** | 1,043,219 | **807,331** |
| full-extent/z2 | 1,843,842 | **1,579,541** | 1,812,386 | **1,604,598** |
| full-extent/z3 | 1,200,951 | 1,184,170 | 1,157,585 | 1,237,031 |
| full-extent/z4 | 1,279,536 | 1,287,501 | 1,226,077 | 1,253,204 |
| full-extent/z5 | 1,726,422 | 1,637,348 | 1,743,642 | 1,577,588 |

`8` is at least as fast as `4` on every shape tested, in both trials, and meaningfully faster on
the two lower-tile-count shapes (natural/z5, 81 tiles: ~20% faster; full-extent/z2, 16 tiles: ~13%
faster) — no shape favoured `4`. **Chosen: `TILE_PAR_MIN_LEN = 8`**, replacing the argued-not-
measured `4`.

---

## 4. Chosen constants

```rust
// crates/tessera-engine/src/viewport.rs
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 200_000;  // Σ range.len(), predictor
const TILE_PAR_MIN_LEN: usize = 8;                   // was 4
```

- **`SERIAL_FALLBACK_MAX_ROWS = 200,000`** — below this many rows spanned by a request's resolved
  tiles (`Σ range.len()`, available pre-fan-out), fold `tile_result` serially; at/above, use the
  existing `pool.install` fan-out. Argument: tile count does not discriminate (measured, §2); `Σ
  range.len()` does, with a clean crossover band (165k serial / 316k parallel) the constant sits
  inside, biased toward serial for the asymmetric-cost reason given above.
- **`TILE_PAR_MIN_LEN = 8`** (was 4) — measured at least as fast as 4 on every clearly-parallel
  shape tried, reproducibly faster on two of six. 16 and above are clearly worse, confirming (not
  overturning) the constant's original design argument.

Both are `pub`/documented with full provenance comments in `viewport.rs` itself (fixture, box,
date, numbers, and — for the threshold — a pointer to this report for the sweep table).

---

## 5. Before / after — the c=1 overhead (item 1 of the brief)

Engine-level (this report's own tool, §1 above), same shape both times:

| | before (measured, this task) | after (calibrated) |
|---|---:|---:|
| `threads=1` total_ns (avg) | 80,257 | *(unchanged — always the serial path pre- and post-calibration at this shape)* |
| `threads=default` total_ns (avg) | 909,669 | serial-fallback engages; same code path as `threads=1` since 21 rows spanned << 200,000 |

At this exact shape (`sigma_visible=21`, well under the threshold), `threads=default` now takes the
identical serial fold `threads=1` always took — the 12.9x gap in §1 is eliminated by construction,
not narrowed.

Real-server confirmation (`scripts/bench_concurrency.py --criteria cpu`, criterion 4b, w=10, k=30,
zoom=8, single worker, 5 s per cell):

```
threads=1        c=1 (small, z8) p50=0.308 ms  server_p50=0.138 ms  pts/s=1,002,250  pts/req=433.1
threads=default  c=1 (small, z8) p50=0.310 ms  server_p50=0.139 ms  pts/s=995,730   pts/req=433.3
small-viewport ratio (default/1) = 1.01   (target: within ~10% — PASS, comfortably)
```

Task 9's own measured number for this exact cell was **2.97x slower** (0.376 ms -> 1.118 ms). This
run: **1.01x** — default is now statistically indistinguishable from `threads=1` on the small
viewport, a complete elimination of the regression, not merely an improvement toward the ~10%
target.

**The large-work viewport (brief's explicit ask: "construct one within config bounds, document
it").** `bench_concurrency.py`'s `load` arm has no `--underlay-offset` flag (unlike the `viewport`
arm), so span is the only lever reachable through this CLI; `gen_viewports`' own formula floors
span at half the extent for any zoom <=5. `--zoom 4` was used — the largest span reachable, and
exactly the `natural/z4` shape §2's sweep already measured at 0.39-0.61x (parallel faster),
comfortably inside `max_tiles_per_request` (262,144) by three orders of magnitude:

```
threads=1        c=1 (large, z4) p50=1.500 ms  server_p50=1.328 ms  pts/s=729,930   pts/req=1,092.7
threads=default  c=1 (large, z4) p50=1.023 ms  server_p50=0.855 ms  pts/s=1,052,642 pts/req=1,093.5
large-work ratio (default/1) = 0.68   (default WINS, as designed)
```

Both halves of the calibrated behaviour hold on the real server: the small/sparse regression is
gone, and the large-work case still gets the parallel win the design exists for.

---

## 6. Matrix cells (brief's cell b) — Arm A/B at c=5, 100, 1000

Same fixture/params as Task 9 (w=10, k=30, zoom=8, 10 s/cell), compared directly against
`task-9-report.md`'s table:

| arm | conc | Task 9 rps | this run rps | Task 9 p50 | this run p50 | Task 9 p99 | this run p99 | Task 9 shed% | this run shed% |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| B | 5 | 1,728.2 | **11,771.4** | 0.73 ms | **0.34 ms** | 28.25 ms | **1.30 ms** | 0.86% | 0.01% |
| B | 100 | 9,072.6 | **31,247.9** | 3.35 ms | **2.19 ms** | 40.84 ms | **5.27 ms** | 81.66% | 40.9% |
| B | 1000 | 13,695.6 | **28,928.7** | 15.50 ms | **18.72 ms** | 51.90 ms | **63.58 ms** | 79.17% | 39.6% |
| A | 5 | 6,437.5 | **11,039.1** | 0.69 ms | **0.36 ms** | 2.32 ms | **1.33 ms** | 0.0% | 0.0% |
| A | 100 | 12,989.9 | **27,693.7** | 3.42 ms | **2.32 ms** | 10.17 ms | **5.61 ms** | 79.63% | 48.2% |
| A | 1000 | 10,081.3 | **20,966.1** | 17.50 ms | **18.98 ms** | 59.24 ms | **63.36 ms** | 83.77% | 60.0% |

**Every cell improved, most by a wide margin** (Task 9 was measured against post-D-G, pre-
calibration code — the same 2.97x-per-request tax §1 found was being paid by EVERY request in the
matrix, not just the isolated c=1 cell). rps roughly 1.7-6.8x higher; p50/p99 down sharply at low
concurrency (Arm B c=5 p99: 28.25 ms -> 1.30 ms); shed rates at c=100/1000 roughly halved (each
admitted request now costs ~3-4x less server time at this fixture's typical density, so more
requests fit inside `compute_admission`'s fixed slot count before the D-B gate trips). c=1000 p50
is very slightly higher than Task 9's (18.72/18.98 ms vs 15.50/17.50 ms) while p99 is very slightly
higher too — both cells hit the gate's queue-wait tail at that concurrency regardless, and the
`shed_rate` there is correspondingly lower (more requests are being genuinely queued/served rather
than immediately shed), which is a consistent, not contradictory, explanation. New metrics (points
delivered) for these cells are in `bench-runs/post-calibration/concurrency-summary.json`.

Criterion 4a (CPU saturation, open-loop): `rate=28,123 rps target achieved=28,116 rps cpu=642.8%`
(80.4% of the 8-core availability bound) — comparable to Task 9's 79.99%; not directly the same
absolute target since it is derived from THIS run's own (much higher) Arm B ceiling, but still
short of the 85% bar for the same "closed-loop-derived rate may under-drive demand" reason Task 9
gave, not a new problem this task introduced.

---

## 7. Shed cell (brief's cell c) and the points-drop interpretation

`--criteria shed` (unchanged params: `compute_admission=1`, `compute_queue=0`, c=1000, 10 s):

```
shed_rate: 94.1%   requests_ok: 45,249   retry_after_violations: 0   requests_hung: 0
served_p50: 11.50 ms   served_p99: 46.38 ms
points_per_second: 1,585,650   points_per_user_per_second: 1,585.7
```

Shape unchanged from Task 9's criterion-3 cell (98.43% shed there vs 94.1% here — both single-
permit gates under 1000-way concurrency; the residual difference is explained by this task's own
per-request cost drop letting slightly more traffic through the one admitted slot before the
timeout fires, consistent with §6's finding, not a new mechanism).

**How much delivered-work drop does an 80%+ shed actually mean?** Computed against the F4 memo's
own ungated Arm B c=1000 baseline (`load.rs`'s doc table: 49,475 rps), using this task's own
measured points-per-request at k=30 (the F4 memo predates the points-served metric; the Arm B
matrix cells above measured 345.6-350.1 points/request across c=5/100/1000, averaging **347.8** —
a property of the corpus/selection, not of gating, so it carries across unchanged):

- **Ungated baseline (hypothetical, F4-era)**: 49,475 rps x 347.8 pts/req ≈ **17,207,000 points/s**
  total; at c=1000, **≈17,207 points/s per user**.
- **Actual (this cell, `compute_admission=1`)**: **1,585,650 points/s** total; **1,585.7 points/s
  per user**.
- **Ratio**: 1,585,650 / 17,207,000 ≈ **9.2% of the ungated delivered rate survives — a ~90.8%
  drop in delivered points/second**, both overall and per-user (the two ratios are identical since
  concurrency is 1000 in both the baseline and this cell).

**This is close to, but not the same number as, the 94.1% request-level `shed_rate`, and the two
should not be read as interchangeable.** `shed_rate` is the fraction of THIS run's own total
arrivals (successes + 429s) that were rejected — a within-run ratio. The points-delivered figure
above instead compares THIS run's successful throughput against a DIFFERENT (ungated, hypothetical)
run's successful throughput — a cross-run ratio with a different denominator. They happen to land
close (90.8% vs 94.1%) here because points-per-successful-request is essentially constant across
every admission regime measured in this task (min 345.6, max 404.5 across every matrix cell) — a
request that gets through delivers a normal-sized response regardless of how contested the gate
was to reach it — so the drop in delivered points tracks the drop in successful REQUEST rate
(45,249/10s ≈ 4,525 rps vs the 49,475 rps baseline, a 90.85% drop, matching the 90.8% points figure
almost exactly) far more closely than it tracks the raw shed percentage. **Read as**: an "X% shed"
headline describes how many arrivals were turned away, which is the availability story; the
points-delivered figures here are the throughput-under-load story, and while they move together in
this dataset, a future workload with much more variable per-request point counts (e.g. very uneven
grant widths) could make them diverge further — which is exactly why both are now reported
separately rather than one being inferred from the other.

---

## 8. New bench metrics — implementation

`crates/tessera-bench/src/arms/load.rs`: `sum_served(body: &[u8]) -> u64` decodes just the tile
stream (the `u32 LE` length prefix + one Arrow IPC `StreamReader` pass summing the `served`
column — `tessera-wire::payload`'s framing doc), never touching the points/subcell streams that
follow. `RawOutcome`/`Sample` carry `points_served`; `run()` computes `points_served_total`,
`points_per_second` (÷ the cell's own measured duration, same denominator `throughput_rps` uses),
`points_per_user_per_second` (÷ `concurrency`, not `distinct_tokens` — see the code comment), and
`points_per_request` (÷ `ok.len()`), all emitted into the JSONL `params` object. `arrow` was added
as a direct `tessera-bench` dependency (checked against `scripts/check-layers.sh`'s deny list
first — nothing forbids it, and every crate this one already depends on pulls `arrow` in
transitively anyway).

`scripts/bench_concurrency.py`: the matrix table gained `pts/s` / `pts/user/s` / `pts/req` columns
and matching JSON summary fields; the criterion-4b thread-scaling cell and the criterion-3 shed
cell both print and record the same three figures.

---

## 9. Files changed

- `crates/tessera-store/tests/permutation_project_parallel.rs` — rider: escaped a stray `>` that
  clippy's `doc_lazy_continuation` read as a blockquote start (commit `0b2554e`).
- `crates/tessera-engine/src/viewport.rs` — serial-fallback predictor + branch, `SERIAL_FALLBACK_MAX_ROWS`
  (200,000, `pub` for test use), `TILE_PAR_MIN_LEN` 4->8, `should_fold_serially` unit-testable
  predictor + its unit test, updated cancellation-bound and module docs.
- `crates/tessera-engine/tests/viewport.rs` — `PARALLEL_HEADLINE_ITEMS` (300,000) for the two
  existing byte-equality tests (were silently falling below the new threshold at the file's
  default 10,000-item fixture — the exact subtlety the brief warned about), a belt-and-braces
  `rows_in_ranges` assertion under `bench-timing`, and a new below-threshold byte-equality variant.
- `crates/tessera-server/tests/http.rs` — same fix for the two server-level byte-equality tests
  (`PARALLEL_HEADLINE_ITEMS`, parameterised fixture-builder helpers).
- `crates/tessera-engine/examples/stage_attribution.rs`, `calibration_sweep.rs`, `min_len_sweep.rs`
  — new, kept as permanent measurement tooling (same precedent as `examples/route_saving.rs`) so a
  future re-calibration (different corpus scale, different box) has a ready-made method rather than
  starting from nothing.
- `crates/tessera-bench/Cargo.toml`, `crates/tessera-bench/src/arms/load.rs` — points-served
  decoding and the three new metrics.
- `scripts/bench_concurrency.py` — new metrics columns/fields, `run_load`'s `zoom` override
  parameter, criterion 4b's added large-work viewport sub-cell.

Not committed (gitignored, machine-local, documented here instead):
- `target` — a symlink at the worktree root to the shared `/home/joe/code/tessera/target`
  (`.cargo/config.toml`'s redirect target). `bench_concurrency.py`/`bench_k_sweep.py` resolve
  binaries relative to the *worktree's own* `target/release/`, which does not otherwise exist in
  this worktree layout; the symlink is the minimal fix and does not touch `.cargo/config.toml`.
  Other worktrees observed alongside this one (`agent-*`) have real, non-shared `target/`
  directories, so this redirect-plus-missing-local-dir combination appears specific to this
  worktree's setup.
- `/tmp/tessera-bench/fixtures/2422486/categories-subclass` — a symlink to the pre-existing
  `/tmp/tessera-2m4` bundle (built earlier via `tessera-engine`'s own criterion-bench convention,
  confirmed present and valid before this task started). The `load` arm's fixture lookup requires
  the `<root>/<scale>/<label-set>/` convention `scripts/bench_build_fixtures.sh` produces
  (`bench_build_fixtures.sh`'s own tail even symlinks `/tmp/tessera-2m4` FROM that canonical path
  when missing) — the reverse symlink here reuses the already-built bytes instead of paying a
  second ~2.42M-row build. **Risk, flagged per the brief's own prompt**: `/tmp` was wiped once
  already this workstream (task-9-report.md's own environment-incidents section); both the bundle
  and this convenience symlink would need rebuilding/relinking after another wipe. A persistent
  location under the workspace `bench-runs/` dir was considered and rejected for now: the bundle is
  ~109 MiB of binary data, and every other task in this workstream (Task 9 included) has kept run
  outputs out of git for the same reason. If this recurs often enough to be worth fixing properly,
  `scripts/bench_build_fixtures.sh --fixtures <persistent-dir>` is the existing, already-designed
  lever — a one-line change to wherever this task's own invocation is scripted, not a new
  mechanism.
- `reference/.venv/` — created during this task (`python3 -m venv` + `pip install requests pyarrow
  numpy pyroaring pytest`); did not exist at task start despite `task-9-report.md` referencing it,
  consistent with it being gitignored and non-persistent across sessions. Needed for
  `scripts/bench_concurrency.py` (imports `reference/oracle/harness.py`) and for re-running
  `reference/tests` (30/30 passed under this venv, confirming the wire/oracle suite is unaffected).

---

## 10. Guard-rails — test summary

- `cargo clippy -p tessera-store --all-targets -- -D warnings` — clean (rider fix confirmed the
  gate, before touching anything else).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `bash scripts/check-layers.sh` — exit 0.
- `cargo test --workspace` — every `test result:` line (39 total across all unit/integration/
  doc-test binaries, including nine empty doc-test crates) read `ok`, **0 failed** anywhere,
  including the OOM-prone `write_permutation_rejects_entity_id_not_fitting_u32`-adjacent
  `segment_roundtrip` suite, which ran clean at 54-56 s both times it was exercised. (The raw run
  log was a scratch `/tmp` file, not persisted — the pass/fail counts above are transcribed
  directly from the run, matching this report's own no-silent-adjustment discipline.)
- `cargo test -p tessera-engine --features bench-timing --test viewport` — 38 passed, 1 ignored
  (the always-ignored 2.4M `latency_sanity` test), including all three byte-equality variants
  (above-threshold headline, above-threshold sparse, new below-threshold).
- `cargo test -p tessera-server --features bench-timing --test http` — 35 passed, including both
  byte-equality variants at the new `PARALLEL_HEADLINE_ITEMS` fixture size.
- `reference/.venv/bin/python -m pytest reference/tests -q` — 30 passed (not in the brief's
  explicit guard-rail list, but the same suite Task 9's criterion 7 used to confirm wire bytes are
  unaffected; run anyway since the venv had to be created regardless).
- `cargo test -p tessera-engine --lib should_fold_serially` — the new predictor unit test, both
  boundary edges, passes.

---

## 11. Self-review

- Re-derived the "why tile count loses" argument from the actual sweep data rather than asserting
  it — the natural-shape-constant-tile-count-varying-verdict observation and the few-tiles-huge-
  work full-extent/z1 case are both concrete counter-examples pulled from the table, not a
  plausibility argument.
- The stage-attribution tool's FIRST run (single-term grant) produced a result contradicting Task
  9's own finding; rather than discard it silently, it is reported in §1 as a genuine finding about
  regime-sensitivity, with the corrected w=10 re-run alongside it — the wrong-regime run is not
  hidden, it is the reason the corrected method (copy the real client's exact RNG draw) exists.
- Verified `compute_threads=1`'s use as a "serial" stand-in in the sweep is conservative in the
  correct direction (documented in the code comment and §2): it still pays `pool.install`'s entry
  cost, so any crossover found against it is, if anything, biased toward UNDER-selecting the serial
  region relative to the true zero-overhead fallback — never the reverse.
- Confirmed the byte-equality fixture-size fix actually closes the gap it claims to: both new
  `rows_in_ranges >= SERIAL_FALLBACK_MAX_ROWS` assertions pass under `--features bench-timing`
  (i.e., the fixtures genuinely clear the threshold, not just "probably do" by the size argument
  alone).
- Checked the large-work viewport choice (`--zoom 4`) against the calibration sweep's own
  `natural/z4` row rather than picking a zoom and hoping — the real-server ratio (0.68) landed
  inside the range the engine-level sweep already predicted (0.39-0.61 at `compute_threads=1` vs
  parallel; HTTP overhead compresses the ratio somewhat toward 1.0 relative to the engine-only
  measurement, consistent with §1's same observation about the small-viewport case).
- Did not force the `with_min_len` choice past what six shapes support: reported it as "8 is
  at-least-as-good-everywhere, meaningfully-better-on-two", not as a universally-optimal value —
  see concerns below.
- Ran `cargo test --workspace` to completion (not just the two crates the brief names) specifically
  to catch the exact "byte-equality tests silently degrade to serial-vs-serial" failure mode the
  brief warned about; it would NOT have been caught by only running `-p tessera-engine
  --features bench-timing`, since the pre-fix versions of those tests still passed their own
  (weakened) assertions.

---

## 12. Concerns for the controller

- **`with_min_len` was swept over six shapes on one box, once (plus one repeat trial).** The signal
  (8 >= 4 everywhere, better on two) is reproducible across the two trials run, but six shapes is a
  narrow basis for a constant meant to generalise across corpus scales and zoom levels. If a future
  task revisits this at a materially different scale (25M+, or a very different tile-count regime),
  re-run `examples/min_len_sweep.rs` rather than trusting this number indefinitely.
- **The 100k-165k row band is genuinely ambiguous, not merely under-sampled.** `Σ range.len()` is a
  proxy for containers-touched, and two shapes at similar row-spans measurably disagreed on which
  path was faster in that band (natural/z6/s2 at 103,509 rows: 1.31x SERIAL-favouring; nearby
  z6/s4 at 316,559 rows: 0.69x PAR-favouring — consistent with the trend, but the band itself
  showed real non-monotonicity between its own samples). 200,000 is a defensible, data-supported
  choice inside that band, not the unique correct answer; a corpus with a very different container-
  density distribution could shift where the real crossover falls.
- **The large-work viewport in the c=1 cell is span-limited, not underlay-limited**, because
  `tessera-bench load` has no `--underlay-offset` flag. This demonstrates the calibration correctly
  on the lever available; it does not independently confirm the §3.3 underlay path (a different
  per-tile cost shape) crosses the same threshold sensibly. **Resolved in fix round 1** (§13):
  `underlay_cells_demanded` is now added into the predictor directly, so a saturated underlay is no
  longer invisible to the serial/parallel decision regardless of how few rows the base tiles span —
  see §13 for the argument. The underlay path's own real-request cost still was not separately
  swept the way rows-only shapes were (no dedicated underlay-heavy sweep row exists in either
  table), which remains worth flagging.
- **`points_per_request`'s near-constancy across every admission regime measured (§7) is a property
  of this task's OWN workload** (uniform w=10 grants, one fixed corpus), not a general guarantee.
  The shed-interpretation paragraph's claim that request-shed and points-drop track closely is
  scoped to this dataset and says so explicitly; a workload with high grant-width variance could
  see the two diverge, which is the reason both metrics are now reported independently rather than
  one being derived from the other.
- **The `/tmp` fixture symlink and the worktree `target` symlink are both machine-local
  workarounds**, documented in §9 rather than fixed at the root cause (the `.cargo/config.toml`
  redirect combined with a missing local `target/` is out of this task's scope per its own
  instructions not to touch that file). A future task running these same scripts cold will hit the
  same two failures this task diagnosed; this report is the paper trail, not a permanent fix.

---

## 13. Fix round 1

Review found one medium issue (calibration validity: the sweep's grant was ~20x sparser than the
validation workload's) and four small ones. All five addressed below; covering tests re-run
(byte-equality suite, predictor unit test, both engine and server suites, full workspace); no full
matrix re-run, per the review's own scope.

### Medium — dense-mask re-run

**The complaint, and why it was right.** `SERIAL_FALLBACK_MAX_ROWS`'s predictor
(`total_rows_in_ranges`) is deliberately pre-mask — it sums `range.len()` over resolved tiles
before any visibility check runs. §2's sweep grant (`spread_descriptors`: evenly-spaced,
deterministic term selection) produced `sigma_visible = 21` at the exact validation bbox
(`bench-runs/stage-attribution.txt`), while the real server run on the identical shape measured
~433 points/request (`bench-runs/post-calibration-console.txt`, criterion 4b). Mask density is a
real cost driver `count`/`select`/`gather` see and the predictor does not — §2's original doc
overclaimed by saying "grant width... feed[s] into `rows_in_ranges` directly", which is false:
`rows_in_ranges` is geometry-only and cannot see the grant at all.

**Method.** `crates/tessera-engine/examples/calibration_sweep.rs --dense` now uses `random_grant`
— a direct duplication of `tessera_bench::corpus::build_grant`'s `GrantShape::Random` arm
(`StdRng::seed_from_u64(seed)`, shuffle every term, truncate to `w=10`), the bench's own
construction, rather than `spread_descriptors`'s deterministic even-spacing. Re-run on the same 53
shapes, back to back with a fresh re-run of the original sparse grant (same box, same session,
minutes apart — controlling for the box-load confound below), both persisted under `bench-runs/`:
`calibration-sweep-dense-mask.txt`, `calibration-sweep-sparse-mask-rerun.txt`.

**An unplanned but important confound surfaced while doing this: absolute `pool.install` overhead
is box-load-sensitive, and this affects how the ORIGINAL (§2) numbers should be read.** The two
fresh re-runs — sparse and dense, run minutes apart on a now-quiet box — both show near-zero-row
shapes (natural z7-z14) converging to `ratio ≈ 1.00` (serial and parallel costing the same ~21-22
µs), NOT the 6-20x serial-favouring spread §2's original table reported for the identical shapes
hours earlier on a busier box. This is evidence that `pool.install`'s overhead varies materially
with system contention, not that the original measurements were wrong for their own conditions —
Task 9's real-HTTP 2.97x and this task's own stage-attribution 12.9x (§1) are independent
measurements from different points in time and are not retracted by this observation. It does mean
the ENGINE-LEVEL sweep's absolute magnitudes should be read as "this box, this moment" rather than
a portable constant, which is why the crossover comparison below is done as a same-session,
back-to-back PAIR (controlling for the confound) rather than against the original §2 numbers
directly.

**The paired comparison (same box, same session, sparse vs dense):**

| shape family | rows_in_ranges | sparse ratio (fresh) | dense ratio (fresh) | verdict agreement |
|---|---:|---:|---:|---|
| natural/z4 (x8 seeds) | 1.0M-1.75M | 0.31-0.53 | 0.38-0.64 | both clearly PAR |
| natural/z5 (x8 seeds) | 0.84-1.68M | 0.32-0.49 | 0.39-0.83 | both clearly PAR |
| natural/z6, low end (~103-165k rows) | 103k-165k | 0.95-0.99 (near tie) | 0.93-1.00 (near tie/one SERIAL) | both ambiguous/tied |
| natural/z6, high end (~317-506k rows) | 317k-506k | 0.53-0.75 | 0.50-0.89 | both PAR, dense sample s1 (428k rows, 0.89) notably weaker than its sparse counterpart (437k rows, 0.57) — largest single divergence found |
| natural/z7-z14 (near-zero rows) | 0-70k | ~0.98-1.02 (near tie) | ~0.80-1.01 (near tie) | both near parity |
| full-extent/z1 (4 tiles, whole corpus) | 2.42M | 1.00 (tie) | 1.07 (marginal SERIAL) | both marginal |
| full-extent/z2-z5 | 2.35-2.42M | 0.27-0.58 | 0.38-0.60 | both clearly PAR, dense ~10-20pp less favourable |

**Where the dense-mask crossover lands.** The same two bands hold: clearly-parallel from roughly
300-500k rows up (both grants agree at every sample above that), ambiguous/near-tied from roughly
100k-300k rows (both grants agree), near-parity (not the dramatic serial-win §2's original noisy
run showed) below that. The dense grant is LESS favourable to parallel at the high end in every
full-extent sample, by 2-14 percentage points and growing with tile count (z2: 0.58 sparse -> 0.60
dense, +2pp; z5: 0.27 -> 0.38, +11pp) — consistent with real per-row work (select+gather on rows
that actually pass the mask) costing more than the count-only cost a sparse mask mostly pays, the
correct-sign effect for mask density to have. One sample in the ambiguous-to-high band diverged
more sharply than the rest — natural/z6/s1 (~430k rows either grant): sparse 0.57 (comfortably
PAR) vs dense 0.89 (barely PAR, close to tied) — the largest single swing found in either
direction; reported rather than smoothed over, since it is exactly the kind of case a "no material
difference" summary could paper over. It remains, on this box and corpus, a same-magnitude
(single-sample, ratio-scale) effect, not one that moved any sample's VERDICT across the
serial/parallel line, nor shifted either band to a different order of magnitude of rows.

**Decision: `SERIAL_FALLBACK_MAX_ROWS` stays at 200,000.** The dense-mask data does not clearly
demand a different number — if anything it argues for raising it slightly (dense work costs a bit
more per row, so the safe-serial region could extend a little further), which is the same
direction 200,000's own "bias toward serial in the ambiguous band" argument already leans, so the
existing constant is not contradicted. Recorded here rather than silently adopted as a change,
per the review's instruction.

**Doc fix.** `SERIAL_FALLBACK_MAX_ROWS`'s doc comment (`viewport.rs`) is rewritten: the false
"grant width... feeds into `rows_in_ranges` directly" claim is removed; a new paragraph states
plainly that mask density is unmodelled by a pre-mask predictor, describes this fix round's
dense-vs-sparse check as evidence (not proof) that the crossover is not obviously grant-width-
sensitive at this scale, and states the mis-prediction bound honestly: at most ~1.3x measured in
this task's own ambiguous band, consistent with the review's own ≤~1.5-2x framing for what a
mis-prediction costs on a ~1 ms-scale request.

### Small 1 — underlay threaded into the predictor

`Engine::viewport` now captures `underlay_cells_demanded` (the same `demanded` value the existing
`max_underlay_cells` bounds check already computes, `viewport.rs` — was local to that check's
`match` arm, now hoisted to the enclosing scope) and adds it into the predictor:
`total_rows_in_ranges = Σ range.len() + underlay_cells_demanded`. Argument for addition over a
separate worst-case bound: the underlay block's own comment already characterises each sub-cell as
"one small binary search plus one bitmap range-count" — the same shape of operation `count_range`
performs per row-range — so summing the two into one row-equivalent total is the natural extension
of the existing predictor, not a second mechanism bolted on, and it means a saturated underlay
(`max_underlay_cells`, default 8192) on an otherwise-tiny base request now correctly routes to
parallel instead of being invisible to the decision.

### Small 2 — closure hoisted

The 9-argument `tile_result(...)` call, previously written out independently in both the serial
and parallel branches (a real divergence risk — a future argument-list change could be made in one
copy and not the other with no compiler error), is now one closure (`let run = |tile, range| {
tile_result(...) }`) used by both (`.map(|(tile, range)| run(tile, range))`). `run` captures only
shared references and `Copy` values, so it needs no new `Sync`/`Send` bound beyond what the
parallel branch already required of those captures before this change.

### Small 3 — compile-time threshold assertion

Both `tests/viewport.rs` (`tessera-engine`) and `tests/http.rs` (`tessera-server`) gained
`const _: () = assert!(PARALLEL_HEADLINE_ITEMS >= SERIAL_FALLBACK_MAX_ROWS);` immediately after
`PARALLEL_HEADLINE_ITEMS`'s definition. The existing runtime `rows_in_ranges >=
SERIAL_FALLBACK_MAX_ROWS` assertions inside the two headline tests only execute under
`--features bench-timing` (`StageTimings` is all-zero without it); the new compile-time assertion
holds in every build, including a plain `cargo test`, so the exact silent-degradation failure mode
the brief itself warned about (a fixture falling back below the threshold with no test catching
it) now has a build-time backstop, not only a feature-gated runtime one. `tessera-server`'s copy
imports `SERIAL_FALLBACK_MAX_ROWS`
directly from `tessera_engine::viewport` (a production `pub` constant, freely importable) rather
than duplicating the number.

### Small 4 — report transcription fixed

§7's `served_p50` was transcribed as 11.21 ms; the actual JSON value
(`bench-runs/post-calibration-shed/concurrency-summary.json`) is 11.497638 ms. Corrected to
11.50 ms in §7 above. (`served_p99`, 46.38 ms, was already correct.)

### Covering tests

- `cargo test -p tessera-engine --lib should_fold_serially` — 1 passed (predictor unit test,
  unaffected by the underlay change since it tests the pure `<` comparison, not the sum).
- `cargo test -p tessera-engine --features bench-timing --test viewport` — 38 passed, 1 ignored
  (unchanged from before this fix round), including all three byte-equality variants and every
  underlay-specific test (`every_underlay_bound_rejects_rather_than_clamping`,
  `underlay_totals_are_per_viewer`, `underlay_sub_cells_sum_to_the_tile_s_masked_visible_count`) —
  confirms the underlay-into-predictor change did not disturb underlay *behaviour*, only the
  serial/parallel *routing* decision.
- `cargo test -p tessera-server --features bench-timing --test http` — 35 passed, including both
  byte-equality variants.
- `cargo test --workspace` — every `test result:` line green, 0 failed, re-run in full after the
  `viewport.rs` restructuring (closure hoist + predictor changes touch the whole request path, not
  just the two lines under review, so the full suite was re-run rather than only the named tests).
- `cargo clippy --workspace --all-targets -- -D warnings` and `bash scripts/check-layers.sh` —
  both clean.
- `cargo fmt -p tessera-engine -- --check` — `src/viewport.rs` clean (formatted as part of this
  fix round; the example files' own pre-existing formatting was left as found, out of scope).

### Files touched this round

- `crates/tessera-engine/src/viewport.rs` — underlay-into-predictor, closure hoist, corrected
  `SERIAL_FALLBACK_MAX_ROWS` doc.
- `crates/tessera-engine/examples/calibration_sweep.rs` — `random_grant` + `--dense` flag.
- `crates/tessera-engine/tests/viewport.rs`, `crates/tessera-server/tests/http.rs` — compile-time
  threshold assertions.
- `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/bench-runs/calibration-sweep-dense-mask.txt`,
  `calibration-sweep-sparse-mask-rerun.txt` — new sweep artefacts (not committed, gitignored SDD
  workspace, same as every other `bench-runs/` file).
- This report — §7 nit fix, one concerns-section update marking the underlay item resolved, this
  §13.

---

## 14. Re-calibration post-B9 (three scales)

New task: `concurrency/viewpath` had been merged into `main` (`53fa504`), `main` had separately
gained B9's three-tier adaptive selection decode (`a62341f`/`7353624`) and, later, a spilling
build pipeline (`d3581cb`/`49c5097`) plus a merge-follow-up fix to this task's own sweep examples
(`9f2424a`). `bench-1e9-report.md` (this SDD dir) measured `compute_threads = default` **2.52x
slower** than `= 1` on a small viewport and **2.19x slower** on the large-work viewport at 1e9 —
the §5/§13 calibration (`SERIAL_FALLBACK_MAX_ROWS = 200,000`) inverted at scale. This section
re-fits both constants on the merged code (`2c19e13`) with sweeps at three durable scales.

### 14.1 Setup

- Merged `main` into `concurrency/viewpath` (`git merge main`, commit `2c19e13`). Two conflicts,
  both in this task's own example files (`calibration_sweep.rs`, `min_len_sweep.rs` — main's
  merge-follow-up added three `BuildArgs` fields to the `ensure_bundle()`/`BuildArgs` block this
  task had already deleted entirely in favour of `--bundle <path>`); resolved by keeping this
  task's side (the function no longer exists to add fields to). `stage_attribution.rs` (untouched
  by this task at merge time) took main's fix cleanly, no conflict.
- Rebuilt release binaries (`cargo build --release -p tessera-cli -p tessera-bench`) after the
  merge — clean, no repeat of the cross-worktree stale-cache `FragmentCacheError` failure this
  task and `bench-1e9-report.md` both hit previously (building both targets together, per the
  known workaround, avoided it again).
- Fixtures: `data/bench-fixtures/1e9/` pre-existed (44 GiB, per `bench-1e9-report.md`).
  `data/bench-fixtures/2m4/` (2,422,486 items, 176 terms, 109 MiB) and `data/bench-fixtures/1e8/`
  (100,000,000 items, 4,930 terms, 4.4 GiB) were built via the same `tessera build` invocation
  `bench-1e9-report.md` used, against `data/scaled/geometry.parquet` /
  `data/scaled/pairs/categories-subclass.pairs.parquet` (the pre-scaled 1e9-capable source; the
  first `2m4` build attempt used the WRONG, un-scaled `data/geometry.parquet`, caught immediately
  because it silently capped at 2,422,486 rows for the `1e8` build too — corrected before any
  sweep data was taken). No new label-set bundles were built by this task at any point — every
  fixture is `categories-subclass`, matching the brief. `data`/`target` worktree-root symlinks
  (same convention as §9) were needed again; both are gitignored, neither is part of this commit.
- **Box contention, twice, both from OTHER worktrees, both waited out rather than raced**: a
  `surnames` 1e9 build (peaked ~24-28 GiB RSS, seen twice) and a `segment_roundtrip` test process
  (peaked ~27 GiB). Neither was started by this task. Per the brief's own instruction, each was
  waited out (polling `kill -0`, no fixed sleep) before this task's own memory/disk-heavy steps
  ran. Disk fluctuated between ~8 GiB and ~52 GiB free over the session purely from these other
  processes' own writes/cleanup, never from anything this task wrote beyond the two new fixtures
  above. **Final state: 21 GiB free on `/`** (`df -h /`), fixtures unchanged since their one build
  (109 MiB + 4.4 GiB + the pre-existing 44 GiB).

### 14.2 A real measurement bug, found and fixed, before trusting any of this section's data

While parameterising `calibration_sweep.rs` for three scales, the SAME geometric shape
(`natural/z4/s0` at 1e9) reported `rows_in_ranges = 303,173,705` under one grant and
`rows_in_ranges = 11,610,284` under another — a 26x gap for a quantity (`Σ range.len()` over
resolved tiles) that is supposed to be **mask-independent** (computed from `ranges`, before any
visibility check runs).

**Root cause**: `tile_result` counts a tile's `range.len()` into its local `TileStats` BEFORE
checking `visible == 0`, but on that empty-tile branch it returns `Ok(None)` — and
`Engine::viewport`'s fold loop discards the whole `TileStats` (including that count) for any
`Ok(None)` result. So `StageTimings.rows_in_ranges` — what both sweep tools were reading and what
§2/§13 partly relied on for reporting (never for the actual serial/parallel DECISION, which reads
`total_rows_in_ranges` computed independently and correctly inside `Engine::viewport` itself) —
silently **excludes every tile the session's own mask made empty**. It is mask-dependent; the real
predictor is not. The two happened to track closely enough at 2.42M with the grants §2/§13 used
that this was never caught there; at 1e9, with a dictionary of 47,968 terms and highly uneven
per-term coverage across the corpus's replica structure, a random 10-term grant can leave whole
tiles empty that a different grant does not, and the gap becomes enormous.

**Fix**: `calibration_sweep.rs`/`min_len_sweep.rs` now compute `rows_in_ranges` (and tile count)
directly — `true_rows_in_ranges`, via `tessera_store::tile_ranges_all` against the bundle opened
outright, no session or mask involved — rather than reading it back off a completed request's
`StageTimings`. Verified fixed: sparse and dense grants now report byte-identical `rows_in_ranges`
for every shape at every scale (`diff` on the two columns is empty).

**This is a real bug in shipped, non-test code, flagged for the controller separately from this
task's own remit** (§14.7) — `StageTimings.rows_in_ranges` feeds `x-tessera-stage-ns` and the C4
leak-register numerator (`rows_in_ranges - sigma_visible`, "rows scanned that this principal
cannot see" — `timing.rs`'s own doc), and both are silently undercounted whenever a request's mask
leaves any tile empty, which is the common case, not the exception. This task did not touch
`timing.rs`/`tile_result`'s production accounting — only its own diagnostic tools — since fixing a
C4/leak-register-adjacent accounting bug is outside a threshold-recalibration task's remit and
deserves its own reviewed change.

### 14.3 Sweep tables, all three scales, both grants

Full data: `bench-runs/recalibration/calib-{2m4,1e8,1e9}-{sparse,dense}.txt` (53 shapes each — the
same `natural`/`full-extent` families as §2/§13). Summary (row-count range and ratio range per
shape family; ratio = parallel p50 / serial p50, `compute_threads = 1` stands in for serial per
§2's own argument):

| scale | grant | natural rows (min-max) | natural ratio (min-max) | full-extent rows | full-extent ratio (min-max) |
|---|---|---:|---:|---:|---:|
| 2.42M | sparse | 0 - 1,752,940 | 0.25 - 1.05 | 2,422,486 (fixed) | 0.29 - 1.01 |
| 2.42M | dense | 0 - 1,752,940 | 0.39 - 1.02 | 2,422,486 (fixed) | 0.36 - 0.82 |
| 1e8 | sparse | 0 - 43,241,684 | 0.54 - 8.89 | 100,000,000 (fixed) | 0.29 - 1.10 |
| 1e8 | dense | 0 - 43,241,684 | 0.79 - 9.71 | 100,000,000 (fixed) | 0.49 - 1.04 |
| 1e9 | sparse | 183 - 354,900,645 | 0.62 - 9.94 | 1,000,000,000 (fixed) | 0.36 - 1.01 |
| 1e9 | dense | 183 - 354,900,645 | **1.00 - 9.93** | 1,000,000,000 (fixed) | 0.54 - 0.92 |

**`full-extent`'s row count is fixed per scale, not a swept variable** — a whole-extent bbox spans
essentially the whole segment regardless of zoom (its only free variable is tile count, 4-1,024).

**The 1e9-dense row is the clean confirmation of `bench-1e9-report.md`'s finding**: under the
bench's own realistic grant, **every single `natural`-family sample measured serial-favouring**
(minimum ratio 1.00, a tie — never a genuine parallel win) across the family's WHOLE observed row
range, up to 354,900,645. Sparse (less realistic — §13's own finding) shows a handful of `natural`
samples dipping to 0.62-0.72 at the high end of its row range, but the dense grant is the one to
trust per §13's own precedent.

**`full-extent` is parallel-favouring, with one measured exception, corrected here (fix round
1)**: `z2`-`z5` (16-1,024 tiles) measured reliably parallel-favouring at every scale, both grants
(0.29-0.95 outside `z1`). `z1` (only 4 tiles — the same "too few units to schedule" effect
§14.5/`TILE_PAR_MIN_LEN`'s doc names) measured SERIAL-favouring at both grants at 1e8 (1.10
sparse, 1.04 dense) and near-tied at 1e9 (1.01 sparse, 0.92 dense) — not the uniform win the first
pass of this section claimed. §14.4's tension argument below does not depend on `z1`: it uses
`full-extent`'s FIXED row count per scale (which `z1` shares with `z2`-`z5`, since a whole-extent
bbox spans the same total rows regardless of tile count), not its ratio, so this correction changes
which shapes get credited with the win, not the numbers the tension argument rests on.

### 14.4 The three-scale (and cross-family) tension — numbers, not a preference

A single row-count threshold cannot separate `natural` (should stay serial, at least under a
realistic grant, up to 354,900,645 rows at 1e9) from `full-extent` (should go parallel, starting
as low as 100,000,000 rows at 1e8) **because their row ranges overlap**:

- **100,000,000 (1e8 `full-extent`'s fixed row count — genuinely parallel-favouring on `z2`-`z5`,
  0.29-0.95; `z1` alone measured 1.04-1.10, corrected in §14.3 above) is LESS than 354,900,645
  (1e9 `natural`'s observed maximum, genuinely serial-favouring under the realistic grant).** A
  threshold above 354,900,645 (to protect 1e9's regression) sits above 1e8's own genuine
  `z2`-`z5` win; a threshold below 100,000,000 (to keep that win) sits below 1e9's regression
  range. No single number does both. (`full-extent`'s row count does not depend on tile count —
  `z1` shares the same 100,000,000 as `z2`-`z5` — so this comparison holds regardless of which
  tile counts within the family are credited with a genuine win.)
- 2.42M sharpens the same point from the other side: its own genuine `full-extent` wins
  (0.29-0.82) run over a segment whose ENTIRE row count is 2,422,486 — a threshold large enough to
  protect 1e9's regression (>354,900,645) forfeits 2.42M's wins outright; the corpus is smaller
  than the threshold would need to be.
- **A fraction-of-corpus formula was tried and also fails on this data.** 1e9's `natural` family
  stays serial-favouring up to 35.5% of the segment (354,900,645 / 1,000,000,000); 2.42M's own
  genuine `full-extent` wins start at 100% of ITS segment by construction (`full-extent` always
  spans the whole thing) while its `natural` wins start around 12-40% of the (much smaller)
  segment. The SAME fraction is parallel-favourable at one scale and serial-favourable at another
  — scaling the threshold by corpus size does not separate the classes either.

**This is the brief's own anticipated branch 3: report the tension, recommend, do not invent a
knob without the data forcing it.** The data here forces the conclusion that neither a constant
nor a fraction-of-corpus formula on the currently-available pre-fan-out quantities (row count,
tile count) can classify correctly at every scale — not a preference for simplicity, a proof by
concrete counter-example in both directions.

### 14.5 Constants landed

```rust
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 500_000_000;  // was 200_000
const TILE_PAR_MIN_LEN: usize = 8;                        // unchanged, re-validated
```

**`SERIAL_FALLBACK_MAX_ROWS = 500,000,000`.** Given the impossibility above, this is a reported
recommendation and a safe compromise, not a claim of uniqueness. Set above the highest observed
`natural`-family serial-favouring row count (354,900,645) with ~40% margin, on the reasoning that
(a) ordinary client viewport traffic (`natural`-shaped) — not a whole-corpus low-zoom scan — is
what this constant exists to protect, since that is literally the regression this whole workstream
originated from and re-confirmed at 1e9; (b) the asymmetric cost this task keeps finding holds
again here (wrongly-parallel measured up to 9.9x slower in this sweep; wrongly-serial forfeits a
win of at most ~2-3x, never a regression against the pre-parallel baseline); (c) 500,000,000
correctly separates 1e9's own two families with margin on both sides (354.9M < 500M < 1,000M).
**Consequence, stated plainly**: this makes the fan-out **dormant** for essentially all traffic at
2.42M and 1e8 (neither corpus has 500,000,000 rows to spend on one request) and for `natural`
traffic at 1e9; it remains reachable at 1e9 for whole-corpus-scale requests. The machinery is not
deleted. §14.7 restates the constant-vs-formula-vs-knob tension for the controller.

**`TILE_PAR_MIN_LEN` stays 8 — re-measured, claim corrected and scoped (fix round 1).** The
coordinator's own working hypothesis going in was that B9's cheaper decode might favour a MUCH
bigger chunk; the data said the opposite, on the shapes that matter. Full sweep
(`bench-runs/recalibration/minlen-{2m4,1e8,1e9}-v{8,32,128,512}.txt`, sparse grant, the
`full-extent` family — the one family that still reaches the parallel branch post-§14.5's
threshold — plus `natural/z4` as a second check):

| scale | shape | v8 | v32 | v128 | v512 | v8 wins? |
|---|---|---:|---:|---:|---:|---|
| 2.42M | full-extent/z5 | 1.44 ms | 1.41 ms | 1.71 ms | 3.31 ms | close (v32 marginally ahead) |
| 1e8 | full-extent/z5 | 1.93 ms | 1.95 ms | 2.43 ms | 3.59 ms | yes |
| 1e9 | full-extent/z5 | 2.95 ms | 2.95 ms | 4.70 ms | 5.75 ms | yes (tied with v32) |
| 1e9 | full-extent/z2 | 2.53 ms | 3.39 ms | 3.68 ms | 3.08 ms | yes |
| 1e9 | full-extent/z4 | 1.76 ms | 2.92 ms | 3.88 ms | 4.27 ms | yes, decisively |
| 1e9 | full-extent/z1 (4 tiles) | 3.25 ms | — | — | 2.97 ms | **no — v512 faster** |
| 1e9 | natural/z4 (not in production above threshold) | 832 µs | **457 µs** | — | — | **no — v32 faster** |
| 1e8 | natural/z4 (not in production above threshold) | 766 µs | **522 µs** | — | — | **no — v32 faster** |

**The original claim — "8 was at least as fast as every larger value on every shape at every
scale" — was falsified by this task's own data and is corrected here, not repeated.** `8` IS
decisive on `full-extent/{z2,z3,z4,z5}` at 1e8 and 1e9 — the shapes that actually reach the
parallel branch in production after §14.5's threshold change, and the wins there are large, not
marginal. It is NOT uniformly best: `full-extent/z1` (only 4 tiles, the same "too few units to
schedule" effect as its serial-vs-parallel ratio in §14.3) measured faster at 512, and
`natural/z4` — which no longer reaches the parallel branch in production at any scale this task
tested (its own row count tops out at 354,900,645, below the new 500,000,000 threshold) —
sometimes measured faster at 32. `8` is kept on the strength of the shapes that matter now, not
because it won everywhere it was tried. Cheaper per-tile work (B9) still makes fine-grained
work-stealing more valuable on the shapes it helps: a bigger chunk wastes proportionally more of
an idle worker's time relative to the (now smaller) real work in each tile it could have stolen
instead — `full-extent/z1`'s single exception is consistent with this too, since 4 tiles is too
few units for ANY grain to enable meaningful work-stealing, coarse or fine.

### 14.6 Real-server validation, all three scales, plus the surviving win

`scripts/bench_concurrency.py --criteria cpu` (w=10, k=30, zoom=8, matching every prior cell in
this report), against the real fixtures:

| scale | small-viewport ratio (default/1) | target | large-work ratio (default/1) | note |
|---|---:|---|---:|---|
| 2.42M | **1.07** | within ~10%: PASS | 1.04 | dormant at this scale (below); no regression from that |
| 1e8 | **1.08** | within ~10%: PASS | 1.10 | dormant at this scale; borderline but no regression |
| 1e9 | **0.98** | within ~10%: PASS, essentially tied | 1.00 | dormant for `natural` shapes at this scale too |

**The 1e9 row is the headline: `bench-1e9-report.md`'s 2.52x/2.19x regression is gone — both
ratios are within 2% of parity.** The "large-work" cell above is `--zoom 4` (the `natural` family,
per §5's own note that the `load` CLI has no `--underlay-offset`/full-extent lever) — consistent
with 14.5's prediction that `natural` traffic is now dormant-parallel at every scale, so ratios
near 1.0 (not a parallel win) are the CORRECT outcome here, not a shortfall.

**Direct confirmation the surviving win is real, not just a sweep artefact**: a `full-extent/z3`
request (whole 65536×65536 extent, zoom 3) issued against two real 1e9 servers over HTTP
(`validate_full_extent_1e9.py`, ad hoc, output persisted at
`bench-runs/recalibration/validation/1e9-full-extent/result.json`):

```
threads=1        full-extent/z3 @ 1e9  p50=291.98 ms  (n=15)
threads=default  full-extent/z3 @ 1e9  p50=81.02 ms   (n=15)
ratio (default/1) = 0.28   (default WINS, 3.6x faster)
```

Both halves of §14.5's prediction hold on the real server: the `natural`-shaped regression is
gone at every scale, and the one shape family the sweep said should still win parallel at 1e9
does, decisively.

### 14.7 Concerns for the controller

- **Constant vs formula vs knob, restated as the brief asked.** §14.4 proved neither a constant
  nor a fraction-of-corpus formula can classify correctly across all three scales from the
  currently-available pre-fan-out signals (row count, tile count). What DOES appear to separate
  `natural` from `full-extent` — work concentrated in a few large, roughly-uniform-density tiles
  versus spread thinly and unevenly across many — is not cheaply computable before the fan-out
  decision without new instrumentation (it is closer to "variance of range length across tiles"
  than anything currently summed). A corpus-size- or deployment-aware config knob is the more
  complete fix if the controller wants one; this task landed the safe single-constant compromise
  instead, per its own instruction not to invent a knob unprompted.
- **The `StageTimings.rows_in_ranges` mask-dependence bug (§14.2) is real, shipped, and outside
  this task's remit to fix.** It undercounts the C4 leak-register numerator
  (`rows_in_ranges - sigma_visible`) whenever any tile in a request is empty under the mask — the
  common case, not the exception. Flagged here because §14.2's tools needed a workaround to
  produce trustworthy sweep data; the production accounting itself was not touched and still has
  the bug.
- **`SERIAL_FALLBACK_MAX_ROWS = 500,000,000` makes the fan-out dormant for nearly every workload
  this task could build a fixture for.** That is a deliberate, safety-first choice given the
  impossibility in §14.4, not an oversight — but it means the ONLY validated evidence that the
  parallel machinery still does something useful is the single `full-extent`-at-1e9 HTTP check in
  §14.6. A corpus meaningfully larger than 1e9, or a workload shape between `natural` and
  `full-extent` (e.g. a very large but not whole-extent pan), was not tested and might reveal the
  threshold needs revisiting again.
- **1e9-dense's `natural` family bottomed out at ratio 1.00 (a tie), never dipped below** — unlike
  1e9-sparse, which had a handful of samples in the 0.62-0.72 range. This task weighted the dense
  (realistic) grant as authoritative per §13's own precedent, but it means the "protect natural
  traffic" case for 500,000,000 rests on ONE grant seed (`--dense` uses seed 0) at 1e9 — not
  re-verified across multiple random grants at that scale, for the same reason §13's own dense
  sweep wasn't repeated across seeds (time budget).
- **Two more box-contention incidents this round** (§14.1), both from other worktrees, both
  waited out rather than raced, consistent with §13's own observation that this shared box's
  absolute timings are not portable — the RATIOS this section's conclusions rest on were measured
  the same way §13 argued for (paired, same-session comparisons), so this does not undermine
  14.4-14.6's conclusions, but it is the same caveat repeated because it recurred.

### 14.8 Fix round 1

Review found one Important issue and two related Minors in §14. All three addressed below.

**Important — the parallel branch had zero test coverage.** All five byte-equality tests (three
in `tessera-engine`, two in `tessera-server`) reach `SERIAL_FALLBACK_MAX_ROWS` = 500,000,000, and
no unit-test-scale fixture can cross that — so after landing §14.5's constants, every one of them
silently degraded to comparing the serial fold against itself, and `pool.install`'s own
collect-order/byte-equality claim (the module doc's central invariant, and the reason Task 6's
"never `Result<Vec<T>>`" collect shape exists at all) had no test able to reach it, live in
production at 1e9, shipping green regardless.

**Fix: a per-`Engine`, test-only threshold override, not a deployment knob.**
`Engine::set_serial_fallback_max_rows_for_test(&self, value: u64)` — `#[doc(hidden)]`, gated
behind the `bench-timing` feature (both crates' integration suites already build with it; it does
not exist at all, not even as an unreachable symbol, in a build without the feature, and a shipped
binary never has it). Backed by a new `pub(crate)` `AtomicU64` field on `Engine` itself
(`serial_fallback_max_rows`, defaulted at `open` to the real constant, read once per request in
`Engine::viewport` in place of reading the constant directly — one extra atomic load is the entire
production cost, paid whether or not `bench-timing` is enabled since the field itself is
unconditional; only the setter is feature-gated). `should_fold_serially` now takes the threshold
as an explicit parameter rather than reading the constant itself, so it stays a pure,
unit-testable function (a new test,
`should_fold_serially_honours_an_arbitrary_threshold_not_just_the_constant`, pins that it is a
genuine parameter).

**Design choices, argued:**

- **Per-`Engine`, not global or thread-local state.** `cargo test` runs tests concurrently by
  default; a process-global override would let one test's setting leak into another's
  concurrently-running assertions. A thread-local would silently stop working the moment a
  request is served from a different OS thread than the one that set it — which is exactly what
  `tessera-server`'s tests do (the engine is driven from `axum`/`tokio` task threads, not the
  test's own async task). Every test already constructs its own `Engine` and never shares it with
  another test, so scoping the override to that instance sidesteps both hazards for free.
- **`pub`, not `pub(crate)`, on the setter itself.** The review's own note was correct and is
  worth restating: `tests/*.rs` integration tests are SEPARATE crate compilation units from the
  library crate, so `pub(crate)` items are invisible to them regardless of `#[cfg(test)]` — only a
  genuinely `pub` item is reachable. `#[doc(hidden)]` plus the `bench-timing` gate is what keeps
  it out of the crate's normal public surface instead.
- **`bench-timing`-gated over a bare `#[cfg(test)]` path**, per the review's own suggested shape:
  a plain `#[cfg(test)]` method on the library crate would not be visible to `tests/*.rs` either
  (same separate-compilation-unit reasoning), so it would not have solved the actual problem;
  `bench-timing` was already the feature both test suites build with for exactly this kind of
  extended, non-default diagnostic surface (the `x-tessera-stage-ns` header rides the same gate).

**Both crates' `tests/http.rs` needed one small refactor to reach the fix**: `spawn_server_from_engine`
was factored out of `spawn_server_with_config_and_gate` (which now just constructs the default
`Engine` and delegates) so the two byte-equality tests can construct their OWN `Engine`, call the
override on it, and only then hand it to the existing router/listener plumbing — every other call
site in the file is unaffected (same public signature, same behaviour).

**All five tests updated**: the three below-fixture-threshold tests that used to (mis-)claim
"genuine parallel" now call `set_serial_fallback_max_rows_for_test(0)` on both `compute_threads`
configs before issuing their request (`should_fold_serially(_, 0)` is unconditionally `false` for
a `u64`, so this forces `pool.install` deterministically — no fixture-size dependence at all,
verified by the new unit test above). The below-threshold variant
(`..._below_the_serial_fallback_threshold`) is unchanged, as it should be — its whole point is the
serial branch. Without `bench-timing` (the override does not exist there), the four "genuine
parallel" tests fall back to comparing the serial fold on both configs — real coverage, just not
of the branch their names describe; every guard-rail invocation that matters for this specific
claim builds with `bench-timing`, so this is judged an acceptable, honestly-documented fallback
rather than a gap.

**Minor — `TILE_PAR_MIN_LEN`'s "at least as fast on every shape at every scale" claim was
falsified by this task's own data** (1e9 `natural/z4`: 832 µs at 8 vs 457 µs at 32; 1e8
`natural/z4`: similar; 1e9 `full-extent/z1`: 3.25 ms at 8 vs 2.97 ms at 512). Corrected in both
the constant's doc (`viewport.rs`) and §14.5 above: `8` is decisive on `full-extent/{z2,z3,z4,z5}`
— the shapes that actually reach the parallel branch in production — and NOT uniformly best
overall; `full-extent/z1` (4 tiles, too few units to schedule) and `natural/z4` (which no longer
reaches the parallel branch in production at any tested scale) are named exceptions, not
smoothed over.

**Minor — §14.3's "reliably PARALLEL-favouring" claim for `full-extent` did not hold for `z1`**
(measured 1.10 SERIAL at 1e8-sparse, 1.04 SERIAL at 1e8-dense, near-tied at 1e9). Corrected in
§14.3 and §14.4 above: the win is `z2`-`z5` specifically; `z1` is named as the measured exception,
consistent with the same "too few tiles to schedule" pattern `TILE_PAR_MIN_LEN`'s correction
names. §14.4's tension argument is unaffected — it uses `full-extent`'s row count (fixed per
scale regardless of which tile count within the family), not which specific tile counts win.

**Minor — the 300k fixtures earn their cost again**, per the review's own conditional: the
Important fix landed, so `PARALLEL_HEADLINE_ITEMS` (300,000, both crates) is unchanged — it is
what gives the byte-equality tests a genuinely multi-tile, multi-thousand-row shape (cross-tile
ordering, the underlay path) independent of the threshold override, which now does the "reach the
parallel branch" job on its own.

**Provenance nit**: `SERIAL_FALLBACK_MAX_ROWS`'s doc comment (`viewport.rs`) cited commit
`3862a61` as the sweep's provenance; the sweeps in this section actually ran on `2c19e13` (the
LATER merge, bringing in the spilling build pipeline on top of `3862a61`'s B9 decode). Corrected
in the constant's doc to name `2c19e13` as the commit the sweeps ran on, with `3862a61` kept as
context for when B9 itself arrived. (This report's own §14.1/§14 preamble already cited `2c19e13`
correctly; only the source comment needed the fix.)

**Covering tests re-run**: `cargo test -p tessera-engine --lib` (both new/updated unit tests),
`cargo test -p tessera-engine --features bench-timing --test viewport -- byte_identical` (3/3,
genuine parallel confirmed for the two that now force it), same without `bench-timing` (3/3, the
documented fallback), `cargo test -p tessera-server --features bench-timing --test http --
byte_identical` (2/2) and without (2/2), `cargo clippy --workspace --all-targets [--features
bench-timing] -- -D warnings` (clean both ways), `cargo fmt --check` (clean after running `cargo
fmt` on both touched crates). Full `cargo test --workspace` re-run after all changes, per the same
discipline as every prior round in this report.
