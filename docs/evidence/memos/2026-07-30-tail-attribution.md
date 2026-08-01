# Attributing the constant ~40 ms viewport tail (2026-07-30)

## Method

Follow-on to `docs/evidence/memos/2026-07-30-tail-discrimination.md`, which refuted cold page
faults as the cause of the ~38–47 ms p99−p50 gap seen across k=50..1000 in
`docs/archive/plans/bench-baselines/2026-07-29-1e9-k-sweep.json`. This memo tests the leading
remaining hypothesis: that the gap is **between-viewport work variance**, not a per-request
stall — random bboxes at random zooms 4–12 make `mask.count_range` work (Σ`visible` over ~300
tiles, k-independent) vary enormously per request, while only the gather/serialise path scales
with k.

Same bundle (`/tmp/tessera-1e9`, not rebuilt), same principal recipe (w=10,000, seed 0), same
viewport generator (`gen_viewports`, seed+1) as the existing baseline, so results are directly
comparable.

Three new scripts, all against a single boot each:

- `scripts/bench_fixed_viewport.py` — Experiment 1: one viewport (zoom=6, the first draw from the
  baseline's own generator — a representative member of the random population, not a cherry-pick)
  repeated 500 times at k=50 and k=1000.
- `scripts/bench_work_correlation.py` — Experiment 3: 800 random viewports at k=50 and k=1000,
  decoding the **full** Arrow response (not a 50-sample subset) to sum Σ`visible`/Σ`matched` over
  every tile, alongside tiles resolved, points returned, response bytes and `x-tessera-server-us`.
  Extended per the coordinator's note to record Σ`visible` (uuncapped masked density) separately
  from k-capped `points_returned`, and the ratio Σ`visible`/`points_returned` (where k binds).
- `scripts/discriminate_tail.py` (prior work, unchanged) for the outlier check below.

Raw data: `probes/fixed-viewport-repeat.json`, `probes/work-correlation.json`.

No Rust was touched — Experiment 2 (phase decomposition) was not required to reach a verdict (see
below), so the quality gates (`cargo fmt`/`clippy`/`test`/`check-layers.sh`) do not apply to this
change.

## Experiment 1: fixed-viewport repeat vs random sweep

| k | arm | p50 (ms) | p99 (ms) | p99−p50 (ms) | max (ms) |
|---|---|---|---|---|---|
| 50 | **fixed** (500 repeats) | 9.94 | 15.05 | **5.11** | 24.13 |
| 50 | random (baseline, n=500) | 5.30 | 52.71 | **47.42** | 57.79 |
| 1000 | **fixed** (500 repeats) | 29.24 | 42.20 | **12.96** | 63.42 |
| 1000 | random (baseline, n=500) | 41.38 | 82.44 | **41.06** | 918.55 |

(Random-sweep rows are the existing baseline, `docs/archive/plans/bench-baselines/
2026-07-29-1e9-k-sweep.json` — reused rather than re-run, since the geometry and recipe already
match. `response_bytes.mean == response_bytes.max` in every fixed-viewport arm, confirming the
repeated request really was byte-for-byte identical each time — no drift in the "fixed" viewport.)

**The gap collapses, but not all the way.** At k=50 it falls from 47.4 ms to 5.1 ms — a ~9×
reduction, squarely inside "workload variance" territory. At k=1000 it falls from 41.1 ms to
13.0 ms — a real, large collapse (~3.2×), but 13 ms is not "under ~5 ms": there is a genuine
residual, k-scaling stall left over once all geometry variance is removed. Read plainly: **most of
the ~40 ms gap is workload variance, exactly as hypothesised, but a smaller secondary stall
survives at high k and is not explained by Experiment 1 alone.**

## Experiment 3: work distribution and the falsifiable prediction

800 random viewports, k=50 and k=1000, same geometry both times (tiles/Σ`visible` are therefore
identical between the two k rows below — confirmed: `n_tiles_nonempty` and `sigma_visible` means
match exactly across k).

| k | p50 (ms) | p99 (ms) | gap (ms) | tiles (mean/max) | Σvisible (mean/max) | points (mean/max) | Σvisible/points (mean) |
|---|---|---|---|---|---|---|---|
| 50 | 6.37 | 48.33 | 41.96 | 264.1 / 289 | 25.41M / 144.77M | 11,172 / 14,450 | 3,888 |
| 1000 | 37.96 | 80.66 | 42.70 | 264.1 / 289 | 25.41M / 144.77M | 155,420 / 289,000 | 195 |

Correlations of `server_us` against each work measure:

| measure | corr @ k=50 | corr @ k=1000 |
|---|---|---|
| tiles resolved | −0.46 | −0.04 |
| Σ`visible` (== Σ`matched`, no filters in Phase 1) | **+0.83** | **+0.33** |
| points returned | +0.01 | +0.56 |
| response bytes | −0.00 | +0.56 |

Normalised cost:

| measure | @ k=50 | @ k=1000 |
|---|---|---|
| µs per row visible | 0.00046 | 0.00129 |
| µs per point gathered | 1.046 | 0.211 |
| µs per tile | 44.2 | 124.3 |

**The falsifiable prediction held, with a clean and informative twist.** At k=50, where the
Σvisible/points ratio is ~3,888 (i.e. almost every tile is saturated at its k=50 cap —
`points_returned` carries almost no information about how dense the region actually is), latency
correlates strongly with Σ`visible` (0.83) and not at all with `points_returned` (0.01). This is
exactly what the hypothesis predicts: at low k, the k-capped gather is a fixed, small cost per
tile, and the count loop (`mask.count_range` over Morton ranges, whose cost is real work
proportional to region density even though bitmap operations are O(containers touched) not
O(cardinality)) dominates and drives the variance.

At k=1000, the ratio drops to ~195 — points returned is a much less saturated, more informative
measure of density — and the correlation picture **shifts**: Σ`visible`'s correlation drops to
0.33 while `points_returned`/`response_bytes` climb to 0.56. This says the tail's driver moves
with k: as the gather/serialise path is allowed to do more real work per tile, its own
per-request variance becomes a comparable or larger contributor to the tail than the count loop.
**This is the same shift Experiment 1 saw as a residual**: at k=1000 fixed-viewport, Σvisible/tiles
are constant by construction, so the entire 13 ms residual gap must live in the
gather/serialise/alloc/scheduling path — precisely the component whose correlation with latency
rises in Experiment 3's k=1000 row. The two experiments corroborate each other.

`tiles resolved` alone is a poor proxy in both arms (weakly negative) — number of tiles a bbox
touches is confounded with zoom (coarse zooms touch fewer, denser tiles; fine zooms touch more,
sparser ones), so it doesn't track density on its own; Σ`visible` is the correct proxy, matching
the coordinator's note that unmasked "tile count" is not the right regressor.

**Both Σ`visible`/Σ`matched` columns were identical throughout** (Phase 1 has no filters — the
count loop's `visible == matched` invariant), consistent with Reference Sheet R5's scope note in
`viewport.rs`.

## The outliers

The k=1000 sweep in Experiment 3 had a single max of 810.25 ms (`server_us`) — not in the
7.13 s / 1.75 s range from the tail-discrimination memo's arms A/B, but still an isolated,
outlier-shaped spike (one request, not a shift in the bulk distribution) rather than part of the
steady tail. Swap/RSS deltas were not instrumented in this run (Experiment 3's harness records
work measures, not `/proc` counters — that instrumentation lives in `discriminate_tail.py`).
Consistent with the prior memo's finding, these outliers vary in magnitude run to run (7.13 s →
1.75 s → 1.84–1.85 s → 810 ms across the sessions measured so far) and are not being chased
further here — the brief is explicit that they are a different, rarer mechanism from the ~40 ms
steady-state gap this memo is about.

## What this changes for Phase 1 §5's exit criterion

**The uniform-random p99 gate is measuring the input distribution more than the system, but it is
not entirely an artefact.** Two things are simultaneously true and the gate should be rewritten to
say both:

1. At low k (where the exit gate's own default, k=30, sits), Experiment 1 and 3 agree: the ~40 ms
   gap is overwhelmingly workload variance (masked density Σ`visible` swinging over a ~6-order-of-
   magnitude range — 1 to 144.77M — across random bboxes at zooms 4–12). A single p99 number over
   that distribution conflates "the system is slow" with "this run happened to draw some very
   dense, coarse-zoom viewports." **A p99 gate here is unmeasurable by construction** in the sense
   the brief describes: it is a property of the corpus's density distribution folded through a
   uniform-random geometry generator, not a property of the server.
2. At high k, a genuine residual stall (~13 ms at k=1000, fixed geometry) exists and is *not*
   explained by workload variance — Experiment 3's correlation shift (Σvisible → points/bytes)
   points at the gather/serialise/allocation path, matching the untested candidates named in the
   tail-discrimination memo (per-request allocation, `croaring` container materialisation,
   blocking I/O in the async handler, tokio scheduling). This has not been isolated to a specific
   phase — that requires Experiment 2's instrumentation, not run here because Experiment 1 did not
   show a large enough residual at k=30 (the gate's actual k) to justify it as urgent, but it
   should not be closed out as "just variance."

**Recommendation (not a decision):**

- Replace the single "p99 over uniformly-random viewports < 10 ms" gate with **two** measurements
  that separate the two effects above:
  - **A fixed representative-viewport battery**, repeated N times each (Experiment 1's method): a
    handful of viewports chosen to span the corpus's actual density spectrum (e.g. deciles of
    Σ`visible` at a fixed zoom, plus a few zoom levels), each gated on its own p99−p50 and
    absolute p99. This measures genuine per-request stalls with the workload-variance confound
    removed, and is what would have caught the residual 13 ms directly instead of by inference.
  - **A normalised-cost ceiling** — µs per row counted (Σ`visible`) and µs per point gathered —
    reported over the existing random sweep, since Experiment 3 shows these are near-flat and
    small (µs/row ~0.0005–0.0013; µs/point ~0.2–1.0) even where the raw p99 swings wildly. A
    regression here (not the raw p99) is the honest signal that the *system*, not the input mix,
    got slower.
  - If a single random-sweep p99 number is still wanted for a dashboard/headline, report it
    **alongside** its Σ`visible` distribution (or bucketed by Σ`visible` decile) so a reviewer can
    tell a workload-mix shift from a regression at a glance — an undifferentiated p99 alone should
    no longer be treated as the exit criterion.
- Before closing the investigation, Experiment 2 (phase timings: pin/generation resolve, row-
  projection cache lookup, `compose`, `tiles_for_bbox`, the count loop, sampler, gather, Arrow
  serialisation, plus a per-request containers-touched counter alongside Σ`visible` as the
  coordinator suggested) is still worth doing at k≈1000+ specifically to attribute the ~13 ms
  residual to a phase — this memo narrows it to "gather/serialise/scheduling, not the count loop,"
  not to a specific line.

## Summary for the owner

1. **Experiment 1**: fixed-viewport repeat collapses the gap from 47.4 ms → 5.1 ms at k=50, and
   41.1 ms → 13.0 ms at k=1000. Mostly workload variance, not entirely.
2. **Experiment 3**: at k=50, latency correlates with Σ`visible` (0.83) and not `points_returned`
   (0.01) — the falsifiable prediction held. At k=1000, the correlation shifts toward
   `points_returned`/`response_bytes` (0.56) as Σvisible/points saturation eases — consistent with,
   and explaining, Experiment 1's k=1000 residual.
3. **Verdict**: the leading hypothesis is **confirmed as the dominant driver** of the ~40 ms gap,
   especially at low k, but does **not** fully explain the tail at high k. There is a smaller,
   real, k-scaling stall living in the gather/serialise/allocation path, still unattributed to a
   specific phase.
4. **§5 gate**: recommend replacing the raw uniform-random p99 with a fixed representative-
   viewport battery (workload-variance-free) plus a normalised µs/row, µs/point ceiling; if a
   single random-sweep number is kept, report it bucketed by Σ`visible` alongside the headline.
