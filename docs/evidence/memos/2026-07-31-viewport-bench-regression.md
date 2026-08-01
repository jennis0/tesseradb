# Viewport criterion-bench regression: cause, reconciliation with the calibration memo, and a threshold recommendation

**Status:** investigation memo, uncommitted. Establishes cause with measurements; recommends but
does not change anything. Raw runs were made in a scratch worktree against the shared
`/tmp/tessera-2m4` fixture; every figure below is from this box (12-core WSL2), same session,
back-to-back.

## Result

**The cause is `99af9aa` (`SERIAL_FALLBACK_MAX_ROWS` 200,000 → 500,000,000), and the mechanism is
the serial/parallel branch flip — nothing else between the two commits matters.** The bench's
viewport request (zoom 8, bbox `[0, 0, 23170, 23170]`,
`crates/tessera-engine/benches/viewport.rs:196-197`) resolves **8,281 tiles spanning 242,221
rows** (measured directly with the engine's own `tiles_for_bbox` + `tile_ranges_all` on the
fixture; no underlay is requested, so this is exactly the predictor value —
`crates/tessera-engine/src/viewport.rs:683-685`, `:167-177`). 242,221 sat **21% above** the old
200,000 threshold, so before `99af9aa` all three benches took the `pool.install` fan-out; after
it they take the serial fold (`crates/tessera-engine/src/viewport.rs:747-764`, constant at
`:878`).

**Single-variable A/B, the strong form of the evidence.** At `2f1b31b` with *only* the constant
edited back to 200,000 — seams, `rows_in_ranges` fix and all other suspect code left in place —
the regression vanishes entirely:

| bench | `9f2424a` stock | `2f1b31b`, constant→200,000 only | `2f1b31b` stock | stock/AB ratio |
|---|---:|---:|---:|---:|
| `viewport/tile_sweep_k0` | 1.211 ms | 1.286 ms | 2.425 ms | **1.89×** |
| `viewport/gather_k30` | 1.558 ms | 1.581 ms | 3.442 ms | **2.18×** |
| `viewport/gather_k500` | 2.041 ms | 2.160 ms | 4.141 ms | **1.92×** |

(Criterion point estimates, `sample_size = 20`; CI half-widths ±1–3% throughout. My endpoint runs
reproduce the reported table — 1.2210/1.6637/2.1652 and 2.2843/3.2244/3.8097 — within 1–9%.)

**The other suspects are exonerated by the same table.** Column 2 vs column 1 is the *combined*
cost of everything else in the range — the seam carves `e985afb`/`860a2fe`/`000fd97`
(`PinManager::resolve`, the `RowProjectionCache` wrapper) and the `e486bba` `rows_in_ranges` fix:
**+6.2% (k0), +1.4% (k30, criterion: "no change detected", p = 0.64), +5.8% (k500)** — against a
+87–94% regression. I did not decompose the ≤6% residual further (some of it is plausibly code
layout rather than the seams); it is immaterial to the finding. `e486bba` in particular is
cost-neutral by construction: the `Σ range.len()` sum already existed for the predictor, and the
commit only *moved* a per-tile probe count out of `tile_result` (its diff shows one deleted
`stats.count` per tile and no new work).

## Is the calibration memo's result 2 wrong?

**Yes, as stated — and its own source report says so.** Result 2
(`docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md:18-20`) claims *"serial wins every
natural viewport shape, at every scale, under both grant densities … 1.4×–10× faster than the
pool on all of them."* The calibration report's own §14.3 table
(`probes/2026-07-31-concurrency-workstream/calibration-report.md:719-726`) shows the opposite at
the two smaller scales: at 2.42M the natural family's parallel/serial ratios span **0.25–1.05
(sparse) and 0.39–1.02 (dense)** — parallel *wins* most samples — and at 1e8 the minima are
0.54/0.79. The raw sweep rows are unambiguous (e.g. `natural/z6/s0` at 2.42M dense: ratio 0.59,
verdict PAR — `bench-runs/recalibration/calib-2m4-dense.txt:22`). The "serial wins everything,
1.4–10×" claim describes only the **1e9-dense** row (ratios 1.00–9.93). Result 2 over-generalised
the 1e9 finding to all three scales; the report's §14.5 (`calibration-report.md:790-806`) and the
memo's own Choices table knew better — both state plainly that 500M makes the fan-out **dormant**
at 2.42M/1e8 and *"forfeits 2.42M's wins outright"*. The criterion regression is that documented
forfeit, landing on the project's own regression gate — which nobody ran during the §14 work (no
criterion output exists in `bench-runs/recalibration/`; validation used
`scripts/bench_concurrency.py` at w=10, k=30, zoom 8 on a *small* viewport —
`calibration-report.md:835-842`).

Result 4's validation ratios (1.07/1.08/0.98) do not contradict this: with the threshold at 500M
*both* arms of that comparison take the serial fold at 2.42M/1e8 (and for natural shapes at 1e9),
so ratios near 1.0 are serial-vs-serial ties. The report says exactly that
(`calibration-report.md:844-848`); they validate "the old parallel-by-default regression is gone",
not "serial is optimal".

**The concrete difference between the sweep and the criterion bench — why the sweep never saw
this shape.** The sweep's natural family is a *fixed-size window* per zoom (span =
extent / 2^(zoom−4), `crates/tessera-engine/examples/calibration_sweep.rs:148-178`), which
resolves a near-constant **~289 tiles at every zoom**; its full-extent family tops out at
**1,024 tiles** (z5). The criterion bench's shape is a *large window at a fine tile depth*:
23,170 of 65,536 units at depth 8 (a 256×256 grid, `crates/tessera-spatial/src/morton.rs:154`,
`:182-190`) covers tile coordinates 0..=90 per axis → **91² = 8,281 tiles** — 8× beyond anything
the sweep ever measured, with only ~10% of the segment's rows. The bench's own comment claims
"~300 tiles" (`benches/viewport.rs:194-195`); it is wrong by 28× (289 tiles is the *sweep's* z8
shape, span 4,096 — the comment appears to have conflated "an eighth of the extent's area" with
that). So the two instruments measured genuinely different shape families, and the family the
sweep measured had tile count held nearly constant *by construction* — which is precisely why the
constant's doc could conclude "tile count does NOT discriminate"
(`crates/tessera-engine/src/viewport.rs:862-871`): within a family where it barely varies, it
can't.

The row-count predictor fails on this shape in both directions. The report's §13 found the
100k–300k-row band "ambiguous/near-tied" and ~430k rows only mildly parallel
(`calibration-report.md:519-527`, `calib-2m4-dense.txt:22-29` — `natural/z6` at 289 tiles,
319k–507k rows: ratios 0.59–1.00); the criterion bench at **242k rows but 8,281 tiles** is a
decisive **~1.9–2.2× parallel win**. Two shapes on the same side of any row threshold sit on
opposite sides of the serial/parallel verdict — the serial fold's per-tile fixed cost
(~2.4 ms / 8,281 ≈ **290 ns/tile** at k=0; a modelled figure from measured aggregates, dominated
by per-tile `count_range` and probe overhead) is real work that `rows_in_ranges` does not see.

## Recommendation on the 500M threshold

**The honest answer is that the predictor needs re-deriving with tile count as a second axis; the
500M row term should stay while that happens.** Specifically:

1. **Keep the 500M row term.** It protects the finding that motivated it — the 1e9 natural-family
   regression (2.52×/2.19× parallel-slower, `bench-1e9-report.md:106`) — and the criterion bench
   does not refute that; both results are real, at different shapes. Lowering the row threshold
   back would re-open the far more expensive failure (wrongly-parallel measured up to ~10×
   slower; wrongly-serial ~2× here).
2. **Add a tile-count arm: take the fan-out when `tiles.len()` exceeds a threshold, regardless of
   rows.** Measured support: *no shape above 1,024 tiles anywhere in the campaign data is
   serial-favouring*, and the one ≥2,048-tile measurement that exists — this bench, 8,281 tiles —
   is parallel-favouring ~2× at 2.42M, the scale the sweep found *least* parallel-friendly. A
   value around 2,048 is consistent with everything measured, but the specific number is
   **modelled, not measured** — between 1,024 (parallel-favouring at 2.42M/1e8 full-extent,
   ratios 0.27–0.38, but that family's rows are the whole segment, so it is not a clean
   tiles-only data point) and 8,281 nothing has been swept.
3. **Sweep the missing axis before landing the number**: extend `calibration_sweep.rs`'s
   `shapes()` with a constant-bbox/varying-depth family (the bench's bbox at depths 8–12 gives
   8,281 → ~2.1M tiles over the same 242k rows) at all three scales, and fit the serial fold as
   `a·tiles + b·rows`. The report's §14.4 "no single threshold exists" argument
   (`calibration-report.md:753-772`) is sound *on its own data* but only proves rows-alone and
   fraction-of-corpus fail; a two-term form was never tested because no swept shape varied tile
   count at fixed rows.
4. **Fix the bench comment** (`benches/viewport.rs:194-195`) — and keep the shape: it is
   accidentally the only high-tile-count probe the project has. Consider *adding* a genuine
   ~289-tile natural-shape bench beside it so the regression gate covers both regimes, and run
   `cargo bench -p tessera-engine` as part of any future re-calibration — it was the missing
   instrument this time.

## What is right that a reviewer might expect to be wrong

- **`e486bba` is correct and free.** The mask-independence fix it makes is real (the C4
  leak-register numerator was undercounted 26× under sparse grants) and it adds no measurable
  cost to the bench (k30: no change detected).
- **The seam carves are within noise** (≤6% combined with everything else in the range, on two of
  three benches; nothing on the third). The "measured as within noise at the time" claim
  survives adversarial re-measurement.
- **The 1e9 finding and the asymmetric-cost argument survive.** Nothing here contradicts
  `bench-1e9-report.md` or §14.5's reasoning that wrongly-parallel is the worse failure; the
  criterion bench adds a missing shape family, it does not overturn the measured ones.
- **The report (as opposed to the memo) is internally honest**: §14.3's tables, §14.5's
  "consequence, stated plainly" dormancy admission, and §14.6's explanation of the ~1.0
  validation ratios all say what actually happened. The defect is the memo's result-2 compression
  of "serial wins at 1e9-dense; the forfeit elsewhere is accepted" into "serial wins every
  natural viewport shape, at every scale" — plus the absence of the criterion bench from the §14
  measurement set.

## Confidence

High. This is a shared WSL2 box (load average ~1.5 at run start), but: the effect (~2×) is ~20×
the run-to-run noise; all three runs were back-to-back in one session against one fixture;
criterion's CIs are ±1–3%; and both of the originally reported endpoints reproduce within 1–9%.
The A/B is single-variable at a single commit, so no bisect ambiguity remains. Figures labelled
*modelled* above (the 290 ns/tile serial cost, the 2,048-tile threshold) are derived, not
directly measured, and are flagged where used.

---

## Closed 2026-08-01 — the regression is recovered, measured

The two-axis sweep this memo asked for was run, the owner adopted a tile-count arm
(`TILE_PAR_MIN_TILES = 4_096`, landed `1089b1a`), and the benched shape — 8,281 tiles — now takes
the fan-out. Track C's Task 5 took a fresh criterion baseline at that commit for its own gate, which
independently closes this investigation:

| bench | pre-§14 (`9f2424a`) | after §14 (`2f1b31b`) | after the tile arm (`1089b1a`) |
|---|---:|---:|---:|
| `viewport/tile_sweep_k0` | 1.2210 ms | 2.2843 ms | **1.2180 ms** |
| `viewport/gather_k30` | 1.6637 ms | 3.2244 ms | **1.7268 ms** |
| `viewport/gather_k500` | 2.1652 ms | 3.8097 ms | **2.1016 ms** |

**All three are back to their pre-§14 values**, two of them within 0.3% and the third within 3.8% —
inside this box's documented ±1–3% criterion CIs and its 1–9% cross-session reproduction band. The
recovery figures this memo originally quoted were *modelled* (they combined a measured stock run
with a forced-parallel run from a different session); these are measured, in one session, on the
shipped predicate.

Nothing here was a defect in the §14 work. The threshold was correct for every shape the campaign
measured; it simply never varied tile count, and this bench sat 8× above the highest tile count the
campaign ever reached. The follow-ups that came out of it are in
[the calibration memo](2026-07-31-tile-parallelism-calibration.md): the label axis was tested too
and does not move the crossover, and contiguity at 1e8/1e9 remains the one untested region.
