# Tile-loop parallelism: what the three-scale calibration found, chose, and recommends

**Status:** landed on main (merge 55eab5f). Constants in `crates/tessera-engine/src/viewport.rs`;
raw sweep data and the full process history archived in
[probes/2026-07-31-concurrency-workstream/](../../probes/2026-07-31-concurrency-workstream/)
(the SDD ledger, per-task reports, and `bench-runs/`). This memo is the decision record; read
nothing else unless auditing.

## Results

**1. B9's tiered decode made pre-mask row count a broken predictor of work.** A natural zoom-4
viewport at 10⁹ spans ~320 M rows-in-range and completes selection **serially in ~135 µs** — the
decode walks the mask at container granularity, so span no longer measures work (the design's own
cost model, "O(containers touched), not O(cardinality)", now holds on the selection path). The
original `SERIAL_FALLBACK_MAX_ROWS = 200_000`, fitted pre-B9, misclassified by three orders of
magnitude.

**2. Serial wins the natural viewport family at 10⁹ under the realistic grant — not at every
scale.** *(Corrected 2026-07-31; the original claim, "serial wins every natural viewport shape, at
every scale, 1.4×–10×", overstated the source report and cost a measured ~2× — see follow-up 5.)*
Under the dense grant at 10⁹ every `natural` sample measured serial-favouring across the whole
observed row range, minimum ratio 1.00, up to 9.93×; that is the regression this workstream exists
to fix and it is solid. At 10⁸ the family is mixed (0.54–9.71). **At 2.42 M the pool wins most
`natural` samples** (0.25–1.05 sparse, 0.39–1.02 dense — [report §14.3](../../probes/2026-07-31-concurrency-workstream/calibration-report.md)),
which is exactly the forfeit §14.5 accepted and the table below still records. The fan-out's
~0.5–1 ms scheduling overhead dwarfs post-B9 tile work only where tile work is small.

**3. Parallelism pays in exactly one regime — whole-corpus sweeps — and pays well.** Full-extent
shapes (the zoom-0/1 "first paint of the whole map"): **0.28× at 10⁹ over HTTP (3.6× faster)**,
0.54–0.72× in-engine at z2–z5. This is also the shape a deployment most wants fast.

**4. Validation after re-calibration:** small-viewport `threads=default` vs `threads=1` ratios
**1.07 / 1.08 / 0.98** at 2.42 M / 10⁸ / 10⁹ (was 2.5–3× *slower* before), with the full-extent
win retained. The regression is gone; the win survives.

**5. A telemetry defect was found and fixed on the way:** `StageTimings.rows_in_ranges` (the C4
leak-register numerator's mask-independent term) had become mask-dependent in the tile-loop
restructure — mask-empty tiles' counts were discarded, a 26× under-count under sparse grants.
Restored to the serial prefix (exact pre-restructure semantics); regression test pins
mask-independence and serial/parallel agreement.

## Choices

| Constant | Value | Argument |
|---|---|---|
| `SERIAL_FALLBACK_MAX_ROWS` | 200 000 → **500 000 000** | The serial-favouring family (natural viewports, up to ~355 M rows at 10⁹) and the parallel-favouring family (full-extent, from ~10⁸ rows) **overlap in row count** — no single threshold separates them perfectly. 500 M sits above every measured natural shape with margin and below full-extent-at-10⁹. Cost: the fan-out is dormant below ~full-corpus-at-10⁹ scale. Deliberate, safety-first. |
| `TILE_PAR_MIN_LEN` | **8 (unchanged)** | Re-swept 8/32/128/512 at all three scales. 8 is decisively best on the shapes that actually reach the pool (full-extent z2–z5 — few, enormous tiles want fine-grained stealing). Named exceptions where larger chunks won (full-extent z1, natural z4) do not ship above the threshold. |
| Test reachability | `#[doc(hidden)]` per-`Engine` threshold override, setter gated behind `bench-timing` | With the threshold at 500 M no fixture reaches the pool, which would have made every byte-equality test silently serial-vs-serial. The override keeps the parallel branch's invariant tests real without adding a user knob. The backing field and its one relaxed load per request exist in every build (documented; gating them would fork the hot path for a negligible saving). |

The parallel machinery is **kept, not deleted**: it is load-bearing for whole-corpus sweeps today
and is the mechanism any future re-calibration re-arms with one constant.

## Recommendations

1. **A corpus-size-aware threshold is the complete fix** the 500 M compromise approximates. If
   full-extent latency at 10⁸-class corpora comes to matter (today it measures ~parity), derive
   the threshold from the bundle's row count at `Engine::open` rather than a universal constant.
   Needs its own argued design; not landed unprompted.
2. **Re-run `calibration_sweep`/`min_len_sweep` whenever the cost model moves**: a new box, a
   corpus with different container occupancy, or any change to the selection decode (a B9-class
   change invalidated the first calibration within a day). The tools are parameterised for
   arbitrary bundles; a sweep is minutes.
3. **The mis-prediction bound is small but real:** the predictor is pre-mask by design, so mask
   density is unmodelled; the measured worst case in the ambiguous band is ≤ ~1.3× on a ~1 ms
   request. Acceptable; recorded so nobody rediscovers it as a bug.
4. **Owed: a two-axis sweep before the predictor is trusted. — DONE 2026-08-01; see "Follow-up 4,
   answered" below, which supersedes this item.** *(Added 2026-07-31.)* §14.4 proves
   no row-count constant and no fraction-of-corpus formula can classify correctly, and its closing
   sentence names "row count, tile count" as the quantities disproved — but **tile count was never
   swept as an independent variable**. The `natural` family holds it near-constant (~289) by
   construction and `full-extent` tops out at 1,024, so no measurement in the campaign exceeds
   1,024 tiles. The one shape above it, the criterion bench at **8,281 tiles / 242,221 rows**, is a
   ~2× parallel win at the least parallel-friendly scale (follow-up 5). That both widens §14.4's
   overlap by three orders of magnitude — the conflict zone now runs from 242 K, not 100 M — and
   suggests a second axis may separate the families where one cannot. **Sweep constant-bbox ×
   varying-depth, at all three scales, before adopting any tile-count arm**; a single point cannot
   fit a threshold, and the ~2,048 figure floated in the regression memo is modelled, not measured.
5. **The measured cost of the 500 M compromise, and one thing still unmeasured.** *(Added
   2026-07-31.)* `cargo bench -p tessera-engine`'s three viewport benches roughly doubled at this
   change (`tile_sweep_k0` 1.22→2.28 ms, `gather_k30` 1.66→3.22 ms, `gather_k500` 2.17→3.81 ms);
   cause confirmed by single-variable A/B in
   [2026-07-31-viewport-bench-regression.md](2026-07-31-viewport-bench-regression.md). **This
   criterion bench was never run during the calibration** — add it to the sweep protocol. The
   forfeit is the anticipated one at the anticipated magnitude (§14.5's asymmetry argument bounded
   wrongly-serial at ~2–3× against wrongly-parallel's 9.9×), so the *choice* stands; the memo's
   headline was what hid it. **Unverified:** §14.5's "never a regression against the pre-parallel
   baseline" is asserted, not measured — the sweep's serial arm is `compute_threads=1`, which still
   pays `pool.install` and so is not the pre-parallel path. Settling it needs a two-point run,
   since B9 (`a62341f`) landed *after* the parallelism (`ebfa1d4`) and a single pre-parallel
   measurement would conflate them.
6. **Unrelated but queued behind this work** (workstream follow-ups, in the SDD ledger): bounding
   the two in-memory single-flight caches before sustained-load/large-corpus deployment; an
   inline-below-threshold server fast path to recover the ~25% small-request closed-loop
   throughput still paid to the off-reactor hop; widening `shed_total` (or documenting it) to
   count cold-build 429s.

---

# Follow-up 4, answered: the two-axis sweep

*(Added 2026-08-01. Raw cells, method and fixture provenance:
[probes/2026-08-01-two-axis-sweep/](../../probes/2026-08-01-two-axis-sweep/). 210 measured cells —
35 shapes × 3 scales × 2 grant densities — every run under `scripts/bench-slot.sh` on a quiet box,
no build concurrent with any measurement.)*

## Recommendation

**Add a tile-count arm: take the fan-out when `tiles.len() ≥ 4,096`, *or* when
`total_rows_in_ranges ≥ 500,000,000` as today. Keep the 500 M row term exactly as it is.**

```rust
// recommended; NOT landed — changing viewport.rs's constants is the owner's decision
const TILE_PAR_MIN_TILES: usize = 4_096;
fn should_fold_serially(rows: u64, tiles: usize, rows_max: u64) -> bool {
    rows < rows_max && tiles < TILE_PAR_MIN_TILES
}
```

**Yes — a two-term rule separates the families where one term cannot.** Measured, over all 210
cells, against the better of the two measured arms per cell (*regret*; arithmetic on measured
medians, not a model):

| rule (parallel iff …) | total regret | cells misclassified | worst single cell |
|---|---:|---:|---:|
| `rows ≥ 500M` — **status quo** | 244.47 ms | 73 / 210 | 4.09× |
| **`rows ≥ 500M OR tiles ≥ 4096`** | **18.38 ms** | **27 / 210** | **2.81×** |
| `tiles ≥ 4096` alone | 28.68 ms | 35 / 210 | 2.81× |
| always parallel | 66.81 ms | 123 / 210 | 62.35× |
| always serial | 302.09 ms | 87 / 210 | 4.09× |

**13.3× less total regret than the shipped predictor**, and both terms earn their place: dropping
the row term costs 56% more regret (it is what catches `full-extent` at 1e9, which has only 16–1,024
tiles), and dropping the tile term is the status quo row. 4,096 is a genuine optimum on this data,
not a round number picked from a range — 2,048 measures 20.37 ms and 8,192 measures 34.55 ms.

**The 1e9 natural regression this constant exists to protect is not reachable by any tile arm, and
nothing here reopens it.** The 354,900,645-row figure the 500 M constant was sized above belongs to
`natural/z4/s6`, which resolves **81 tiles** — from the campaign's own raw output
(`probes/2026-07-31-concurrency-workstream/bench-runs/recalibration/calib-1e9-dense.txt:12`). The
`natural` family is 81 tiles at z4 and 289 at every zoom from 5 up, at every scale, because its span
formula (`extent / 2^(zoom−4)`) is exactly 16 cells wide at every depth. A 4,096-tile threshold sits
14–50× above the whole family. Re-measured directly here: all eighteen `natural` cells are
serial-favouring (2.81–34.04×) and **none changes arm under any tile threshold ≥ 1,024**. The row
term is unchanged, so no row-axis behaviour moves either.

## The evidence

**1. The tile axis has a crossover; it is narrow, and it barely moves with scale or with rows.**
Sweeping constant-bbox × varying-depth (rows fixed to within the edge overhang, tiles ×4 per level),
last serial-favouring tile count → first parallel-favouring, **measured**:

| family | rows spanned | 2.42M dense | 2.42M sparse | 1e8 dense | 1e8 sparse | 1e9 dense | 1e9 sparse |
|---|---|---|---|---|---|---|---|
| `natural` | 0 – 83 M | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — |
| `f01` | 0 – 3.5 M | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f04` | 0 – 10.2 M | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 |
| `f12` | 43 – 55 M | 4225 → 16641 | 4225 → 16641 | 1089 → 4225 | 4225 → 16641 | 1089 → 4225 | 4225 → 16641 |
| `f35` | 242 K – 131 M | 2116 → 8281 | 529 → 2116 | 2116 → 8281 | 529 → 2116 | 2116 → 8281 | 529 → 2116 |
| `f100` | whole corpus | PAR from 16 | PAR from 16 | PAR from 16 | PAR from 16 | PAR from 16 | PAR from 16 |

**Across all 210 cells the highest tile count that measured serial-favouring is 4,225, and nothing
at or above 8,281 tiles measured serial-favouring anywhere.** The conflict band on the tile axis is
`[2116, 4225]`; on the row axis §14.4's is `[242 K, 354.9 M]` — five orders of magnitude wider. Row
count spans 0 → 131 M *within* the families whose tile crossover is identical, which is the
separation §14.4 could not test.

**2. Why tile count is the stable axis and row count is not.** The per-tile *floor* — a tile that
resolves to little or no work — measures **85 → 141 ns across a 400× change in corpus size and both
grant densities**. It is per-tile fixed cost (range setup, `count_range` entry, probe overhead) and
mask-independent by construction. The row coefficient has no such stability: `f100/z2` at 2.42M
costs **0.46 ns/row**, while `natural/z4/s6` at 1e9 costs **0.0019 ns/row** — a 240× spread in the
same coefficient. That is result 1 of this memo restated with a mechanism: after B9's tiered decode,
rows-in-range stopped measuring work. **Tile count did not**, and the campaign could not see that
because it never varied it.

**3. What the recommended rule still gets wrong, in full** (every cell where it errs by >1.5×):

| scale/grant | shape | tiles | rows | rule picks | penalty |
|---|---|---:|---:|---|---:|
| 2.42M sparse | `f100/z5` | 1,024 | 2.42 M | SERIAL | 2.81× (+3.15 ms) |
| 2.42M dense | `f100/z5` | 1,024 | 2.42 M | SERIAL | 2.11× (+1.60 ms) |
| 2.42M sparse | `f100/z4` | 256 | 2.42 M | SERIAL | 2.06× (+1.48 ms) |
| 1e8 sparse | `f100/z5` | 1,024 | 100 M | SERIAL | 1.97× (+2.40 ms) |
| 1e8 sparse | `f100/z4` | 256 | 100 M | SERIAL | 1.95× (+1.75 ms) |
| 2.42M sparse | `f100/z3` | 64 | 2.42 M | SERIAL | 1.82× (+0.90 ms) |
| 1e8 sparse | `f100/z3` | 64 | 100 M | SERIAL | 1.79× (+1.31 ms) |
| 2.42M dense | `f100/z4` | 256 | 2.42 M | SERIAL | 1.64× (+0.75 ms) |
| 2.42M dense | `f12/z9` | 4,225 | 43 | PAR | 1.57× (+0.21 ms) |

**Every residual above 1.6× is `full-extent` at ≤ 1,024 tiles** — a whole-corpus sweep at low zoom,
where the tile axis has nothing to say and the row term is below its threshold because the corpus
is. That is exactly §14.4's tension, undiminished and unaddressed by this work; it is what
recommendation 1 (a corpus-size-aware row threshold) is for, and it is now the *only* thing left in
the residual. **Everything the tile arm itself introduces is ≤ 1.57× and ≤ 0.21 ms absolute**, at
exactly 4,225 tiles, in six cells — an order of magnitude smaller than the errors it removes.

**4. The criterion regression is recovered.** The bench's shape is this sweep's `f35/z8` cell —
8,281 tiles / 242,221 rows, matching the regression memo's independently measured predictor values
exactly — and it is parallel-favouring at every scale and both grants (0.29–0.96). 8,281 ≥ 4,096, so
the recommended rule takes the fan-out there. Criterion baseline re-measured at this commit with the
stock 500 M constant (`probes/2026-08-01-two-axis-sweep/raw/criterion-viewport-stock.txt`):
`tile_sweep_k0` **2.321 ms**, `gather_k30` **3.321 ms**, `gather_k500` **3.936 ms** — reproducing
the regression memo's reported endpoint within 3%, so the box and the effect are both stable.
Against that memo's single-variable A/B on the same shape (1.286 / 1.581 / 2.160 ms with the branch
forced parallel), the recommended rule recovers **~1.8× / ~2.1× / ~1.8×**. *(Recovery figures are
modelled: they combine this run's measured stock numbers with that memo's measured forced-parallel
numbers, which were taken in a different session. The direction and the arm selection are measured;
the exact ratio is not a single-session measurement.)*

## Method note, because it changes how the numbers should be read

The arms here are **not** `calibration_sweep.rs`'s. That tool's "serial" arm is
`compute_threads = 1`, which still enters `pool.install` — its own module doc says so and calls the
resulting crossover conservative. This sweep instead builds two engines with *identical*
`EngineConfig` (same `compute_threads`, same pool) and varies **only** the per-`Engine` threshold
override (`session.rs:569`): `u64::MAX` forces the serial fold, `0` forces the fan-out. Every row is
a single-variable A/B of the exact branch at `viewport.rs:754-770`, which is the form of evidence
`2026-07-31-viewport-bench-regression.md` found decisive. Ratios are consequently not directly
comparable with §14.3's; they measure the shipped branch rather than a proxy for it. Reps are
interleaved between arms rather than run in two blocks.

New instrument: `crates/tessera-engine/examples/tile_axis_sweep.rs`. `calibration_sweep.rs` is
untouched and still reproduces the campaign's tables.

## Two corrections to documents in this repository

**a. `viewport.rs`'s "tile count does NOT discriminate" is now falsified, and its own stated reason
is why.** `SERIAL_FALLBACK_MAX_ROWS`'s doc (`crates/tessera-engine/src/viewport.rs:862-871`) argues
that "the `natural` viewport family resolves a near-constant ~289 tiles at every zoom … yet the
measured verdict at that SAME tile count varies with how many rows those tiles actually spanned".
That is a correct observation about a family in which tile count is constant by construction, and it
does not generalise: swept as an independent variable, tile count discriminates better than row
count does, by 13× in total regret. The paragraph should be revised when the arm lands. **Nothing is
changed in `viewport.rs` by this work** — the threshold constants are the owner's decision.

**b. The bench comment is fixed here.** `benches/viewport.rs`'s "on the order of 300 tiles" (wrong
by 28×; 289 tiles is the *sweep's* z8 shape, not the bench's) is corrected to the measured 8,281
tiles / 242,221 rows, with a note on why the mislabelling mattered — while it stood, the project's
only above-1,024-tile probe was filed as a ~300-tile shape and nobody noticed the calibration had
never measured above 1,024. This is the regression memo's recommendation 4, minus the "add a second
~289-tile bench" half, which is a change to the regression gate and so a controller decision.

## What remains open

- **The `full-extent`-at-small-corpus residual** (item 3's table) needs recommendation 1's
  corpus-size-aware row threshold, not another axis. It is now cleanly isolated: it is the entire
  residual above 1.6×.
- **Follow-up 5's unverified claim is still unverified.** "Never a regression against the
  pre-parallel baseline" remains untested; this sweep's serial arm is the true serial fold (better
  than `compute_threads = 1`) but the *pre-parallel* code path is a different commit, and B9 landed
  after the parallelism, so a single-point comparison would still conflate them.
- **`TILE_PAR_MIN_LEN` was not re-swept** at the new tile counts. If a tile arm lands, shapes in the
  4,096–65,536-tile range start reaching the pool for the first time, and `min_len_sweep.rs`'s
  §14.5 table covers only `full-extent` z1–z5 and `natural/z4`. Re-run it before trusting 8 there.
