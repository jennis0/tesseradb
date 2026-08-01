# The label axis — does the tile-count predictor hold across label configurations?

**Result: yes. With coverage held fixed, a 3.8× swing in mask contiguity moves no family's
crossover at all, and the highest tile count that measures serial-favouring is 4,225 in every one
of the three label sets — the same figure the 210-cell two-axis sweep reported. `tiles ≥ 4,096`
survives.**

**What this is.** A check on the recommendation of
[`docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md`](../../docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md)'s
"Follow-up 4, answered". That sweep varied tile count and row count over 210 cells, **all of them
`categories-subclass`**. [`probes/results.md`](../results.md) §5 measures *run ratio* — mean mask
run length over the random-mask expectation — at 1.00 → 5.11 across label sets at a comparable
coverage, and run ratio is contiguity, contiguity is containers-touched, and containers-touched is
the design's own cost model. So the predictor was fitted at exactly one value of a quantity that
feeds per-tile work directly. This probe varies it.

## Method

**Instrument:** `crates/tessera-engine/examples/tile_axis_sweep.rs`, the same tool, unchanged in
its timing path. Three additions, all outside every timed region:

- a **premise block** printing the grant's cardinality, coverage, Roaring container count and run
  ratio, in **entity space and row space**, as a `GRANT,` line (`--contiguity-only` stops there);
- `--target-coverage FRACTION`, which binary-searches the grant width `w` until the credential
  reaches a target coverage, so coverage can be held fixed while the label set supplies the
  contiguity;
- `containers()` and `run_ratio()`, transcribed from `crates/tessera-bench/src/metrics.rs:39,63`
  (the canonical implementations, themselves mirroring the Phase 0 probes' definitions) because
  `tessera-bench` is a binary crate above `tessera-engine` and nothing may depend on it.

**Fixtures:** 2.42M only — the label axis, not the scale axis, which the prior sweep covered at
three scales. `scripts/bench_build_fixtures.sh --scales 2422486 --label-sets
categories-archive,surnames`, plus the existing `data/bench-fixtures/2m4` for
`categories-subclass`. All three are `--limit 2422486` prefixes of the same
`data/scaled/geometry.parquet`; only the pairs file differs.

**Arms** are unchanged: two engines with identical `EngineConfig`, differing only in the
per-`Engine` threshold override (`u64::MAX` = always serial, `0` = always parallel), 25 interleaved
reps after 3 warm-ups, medians. See the [two-axis sweep's README](../2026-08-01-two-axis-sweep/README.md)
for why that is the stronger instrument.

Every run through `scripts/bench-slot.sh` on a quiet box; **no build ran during any measurement**
(fixtures and binaries were built first, in one block).

## Files

| file | what |
|---|---|
| `raw/premise-contiguity-2m4.txt` | the premise check alone, six grants |
| `raw/tiles-2m4-{archive,subclass,surnames}-{dense,sparse}.txt` | **round 1**, fixed `w = 10` grants |
| `raw/round2/…` (same six names) | **round 2**, the replication that supersedes round 1 |
| `raw/cov25/tiles-2m4-{archive,subclass,surnames}-cov25.txt` | the coverage-matched arm — the single-variable one |
| `analysis-{round1,round2,cov25,all}.txt` | `analyse.py`'s output |
| `analyse.py` | crossover extraction and regret arithmetic over measured medians |

Reproduce: `python3 analyse.py raw/round2/tiles-*.txt raw/cov25/tiles-*.txt`.

**Round 1 is kept but not relied on.** Its slot log
(`raw/tiles-2m4-archive-sparse.err`, `raw/tiles-2m4-surnames-*.err`) records the box drifting to
loadavg 2.9–4.8 between runs — another session was busy — and one cell moved as a result:
`f04` under `subclass/dense` read `8281 → 32761` in round 1 and `2116 → 8281` in round 2, because
its 8,281-tile cell measured 1.05 (parity) the first time and 0.90 the second. Round 2 re-ran all
six under `BENCH_SLOT_MAX_LOAD=1.0`. Every other crossover in the table is identical between the
two rounds. Headline figures below are round 2 + the coverage-matched arm: **315 measured cells**.

## 1. The premise check — is the axis actually varied? Yes

**Measured**, the grant each sweep runs against, at 2.42M (universe 2,422,486 rows):

| label set | grant | coverage | **row-space run ratio** | row containers | entity-space run ratio |
|---|---|---:|---:|---:|---:|
| `categories-archive` | dense (`Random` w=10) | 47.84% | **5.230** | 37 | 16,791 |
| `categories-archive` | sparse (even spacing) | 38.37% | **3.386** | 37 | 3,794 |
| `categories-subclass` | dense | 15.94% | **1.750** | 37 | 107 |
| `categories-subclass` | sparse | 24.73% | **1.666** | 37 | 136 |
| `surnames` | dense | 0.0051% | **1.025** | 25 | 2.7 |
| `surnames` | sparse | 0.0033% | **1.000** | 27 | 1.9 |
| `categories-archive` | **coverage-matched** | 32.45% | **4.030** | 37 | 6,896 |
| `categories-subclass` | **coverage-matched** | 27.92% | **1.889** | 37 | 118 |
| `surnames` | **coverage-matched** | 25.00% | **1.050** | 37 | 2.8 |

The axis **is** genuinely varied: row-space run ratio spans **1.00 → 5.23**, covering the whole of
`results.md` §5's 1.00 → 5.11 range, and the engine's own numbers corroborate the probes' — §5's
`categories-archive` head-25% figure of 5.11 sits between this run's 4.03 at 32% coverage and 5.23
at 48%. The check is not vacuous.

Three things it also shows, none of them anticipated:

1. **At fixed `w = 10` the grants differ enormously in *coverage*, not only contiguity** — 47.8%
   on `categories-archive`'s 38-term dictionary against 0.005% on `surnames`' 404,104-term one.
   A fixed grant width is not a fixed principal across label sets. That confound is why the
   coverage-matched arm exists and why it, not the fixed-`w` arm, is the single-variable evidence.
2. **Container count barely varies and cannot discriminate at this scale.** 2.42M rows is 37
   Roaring containers in total, and every non-degenerate mask spans all 37. At 2.42M, contiguity
   shows up as run structure *within* containers, never as containers touched. A rerun at 1e8/1e9
   would be needed to vary containers-touched itself.
3. **Entity space and row space disagree by three orders of magnitude.** `categories-archive`'s
   fragment has run ratio **16,791 in entity space** and **5.23 in row space** — signature-sorted
   entity assignment (I9) makes `M_auth` very nearly one contiguous block, and the Morton
   permutation shreds it. `results.md` §5's point 3 ("entity- and row-space serialisations are
   within noise") is about *serialised size*, and is not contradicted; but run structure is not
   size, and for tile work only the row-space figure is the operative one, since a tile is a row
   range.

## 2. The crossover, per family per label set

Last tile count measuring SERIAL-favouring → first measuring PAR-favouring. **Measured.**

**Coverage-matched (25.0% / 27.9% / 32.5% coverage; run ratio 1.05 / 1.89 / 4.03) — the
single-variable comparison:**

| family | rows spanned | `surnames` (1.05) | `categories-subclass` (1.89) | `categories-archive` (4.03) |
|---|---|---|---|---|
| `natural` (289 tiles) | 0 – 27.5 K | 289 → — | 289 → — | 289 → — |
| `f01` | 0 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f04` | 0 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 |
| `f12` | 43 – 125 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f35` | 242 K – 276 K | 529 → 2116 | 529 → 2116 | 529 → 2116 |
| `f100` (whole corpus) | 2.42 M | PAR from 16 | PAR from 16 | PAR from 16 |

**Identical in every cell.** A 3.8× swing in row-space run ratio moves no crossover.

**Fixed `w = 10` (coverage varies with the label set, 0.005% → 47.8%):**

| family | archive/dense | archive/sparse | subclass/dense | subclass/sparse | surnames/dense | surnames/sparse |
|---|---|---|---|---|---|---|
| `natural` | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — |
| `f01` | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f04` | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 |
| `f12` | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f35` | **144 → 529** | 529 → 2116 | 2116 → 8281 | 529 → 2116 | 2116 → 8281 | 2116 → 8281 |
| `f100` | PAR from 16 | PAR from 16 | PAR from 16 | PAR from 16 | **4096 → 16384** | **4096 → 16384** |

The two families that move (`f35`, `f100`) move **in opposite directions to the run-ratio ordering**
— `categories-archive`, the *most* contiguous mask, has the *lowest* crossover — and both moves
vanish in the coverage-matched arm. They are coverage effects, not contiguity effects. The
hypothesis under test, that a more contiguous mask does less work per tile and so pushes the
crossover *up*, is **falsified**: within these cells contiguity does nothing to the crossover, and
what does move it is how many visible rows each tile has to count, which is coverage.

**Over all 315 cells the highest tile count that measured serial-favouring is 4,225 — in
`categories-archive`, in `categories-subclass` and in `surnames` alike — and nothing at or above
8,281 tiles measured serial-favouring in any label set under any grant.** The conflict band on the
tile axis is `[2116, 4225]` on the label axis exactly as it was on the scale axis.

## 3. Regret, all 315 cells

*Regret* = how much slower the arm a rule picks is than the better of the two **measured** arms for
that cell. Arithmetic over measured medians, not a model.

| rule (parallel iff …) | total regret | misclassified | worst ratio | archive | subclass | surnames |
|---|---:|---:|---:|---:|---:|---:|
| always serial | 632.54 ms | 123 | 4.70× | 277.47 | 237.45 | 117.62 |
| always parallel | 115.77 ms | 192 | **65.57×** | 35.36 | 36.50 | 43.92 |
| `rows ≥ 500M` — status quo | 632.54 ms | 123 | 4.70× | 277.47 | 237.45 | 117.62 |
| `rows ≥ 500M OR tiles ≥ 1024` | 51.22 ms | 74 | 14.21× | **19.36** | **16.76** | 15.09 |
| `rows ≥ 500M OR tiles ≥ 2048` | 54.58 ms | 61 | 6.87× | 23.39 | 19.15 | 12.05 |
| **`rows ≥ 500M OR tiles ≥ 4096`** | **49.90 ms** | 55 | **3.16×** | 23.73 | 17.35 | **8.82** |
| `rows ≥ 500M OR tiles ≥ 8192` | 93.36 ms | 42 | 3.97× | 42.66 | 35.08 | 15.62 |
| `rows ≥ 500M OR tiles ≥ 16384` | 114.76 ms | 60 | 3.97× | 53.50 | 41.60 | 19.66 |
| oracle (per-cell best) | 0 | 0 | — | — | — | — |

4,096 is the total-regret optimum on the label axis as it was on the scale axis — **12.7× less
regret than the shipped predictor** — and it is the *only* candidate whose worst single cell stays
under 4×.

## 4. What the rule still gets wrong is the same thing as before

Every residual above 1.6× under `rows ≥ 500M OR tiles ≥ 4096`, all 315 cells (**measured**):

| run | shape | tiles | rows | picks | penalty |
|---|---|---:|---:|---|---:|
| archive/dense | `f100/z5` | 1,024 | 2.42 M | SERIAL | 3.16× (+3.30 ms) |
| archive/sparse | `f100/z5` | 1,024 | 2.42 M | SERIAL | 3.01× (+3.24 ms) |
| surnames/cov25 | `f100/z5` | 1,024 | 2.42 M | SERIAL | 3.00× (+3.48 ms) |
| subclass/sparse | `f100/z5` | 1,024 | 2.42 M | SERIAL | 2.96× (+3.13 ms) |
| subclass/cov25 | `f100/z5` | 1,024 | 2.42 M | SERIAL | 2.95× (+3.03 ms) |
| archive/cov25 | `f100/z5` | 1,024 | 2.42 M | SERIAL | 2.65× (+2.54 ms) |
| archive/dense | `f100/z4` | 256 | 2.42 M | SERIAL | 2.63× (+1.91 ms) |
| surnames/cov25 | `f100/z4` | 256 | 2.42 M | SERIAL | 2.52× (+1.74 ms) |
| archive/sparse | `f100/z4` | 256 | 2.42 M | SERIAL | 2.44× (+1.86 ms) |
| subclass/sparse | `f01/z12` | 4,225 | 0 | PAR | 2.42× (+0.53 ms) |

**`full-extent` at ≤ 1,024 tiles on a small corpus, in every label set** — the memo's already-named
residual, which recommendation 1 (a corpus-size-aware row threshold) is for. The label axis adds
nothing new to it and does not widen it. Everything the tile arm itself introduces stays ≤ 2.42×
and ≤ 0.53 ms absolute.

## 5. Limits of this probe

- **2.42M only.** Deliberate — the scale axis was covered by the prior sweep — but it means
  containers-touched never varied (§1 point 2). If contiguity is going to bite anywhere it is at
  1e8/1e9 where a mask spans thousands of containers and a tile's range can miss them; that is
  untested here.
- **Coverage is matched to ±30%, not exactly.** `categories-archive`'s dictionary has 38 terms, so
  grant width is a coarse dial: 25.0% / 27.9% / 32.5%. A 1.3× coverage spread against a 3.8× run-ratio
  spread. The conclusion — no crossover moves — is robust to that, since coverage moving 0.005% →
  48% at fixed `w` moved only two families.
- **The grants are the sweep's own** (`GrantShape::Random`, deterministic even spacing, and a
  width-searched variant of the first). They are not `results.md` §5's head-25% scenarios, so run
  ratios here are this probe's own measurements, not §5's transplanted.
