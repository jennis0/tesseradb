# The two-axis sweep — tile count as an independent variable

**What this is.** Follow-up 4 of
[`docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md`](../../docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md):
the measurement the calibration campaign never made. §14.4 of
[`../2026-07-31-concurrency-workstream/calibration-report.md`](../2026-07-31-concurrency-workstream/calibration-report.md)
proved that no row-count constant can classify serial-vs-parallel correctly and named "row count,
tile count" as the disproved quantities — but **tile count was never swept**. The `natural` family
holds it at 81–289 tiles by construction and `full-extent` tops out at 1,024, so nothing in the
campaign exceeds 1,024 tiles.

The memo's recommendation is answered here: **a two-term rule does separate the families where one
term cannot.** See the memo's "Follow-up 4, answered" section for the recommendation; this file is
the raw data and the method.

**Every cell here is `categories-subclass`.** The label axis — mask contiguity, which
`probes/results.md` §5 measures at run ratio 1.00 → 5.11 across label sets — is swept separately in
[`../2026-08-01-label-contiguity/`](../2026-08-01-label-contiguity/), over 315 further cells at
2.42M. Result: with coverage held fixed, a 3.8× swing in contiguity moves no family's crossover,
and the highest serial-favouring tile count is 4,225 in every label set, as here.

## Method

**Instrument:** `crates/tessera-engine/examples/tile_axis_sweep.rs` (new; the campaign's
`calibration_sweep.rs` is unchanged and still reproduces the campaign's own tables).

**Shapes:** a crossed grid — bbox anchored at the extent origin with side `f · 65536` for
`f ∈ {1.0, 0.353546, 0.125, 0.044189, 0.015625}`, each resolved at six or seven depths. Holding `f`
fixed and raising the depth multiplies tile count by ~4 while rows-in-range move only by the
shrinking edge overhang (a few percent, printed per row so it can be checked). Holding the depth
fixed and varying `f` moves rows by orders of magnitude at a comparable tile count. `f = 0.353546`
reproduces `crates/tessera-engine/benches/viewport.rs`'s bbox exactly — its `z8` cell resolves
8,281 tiles / 242,221 rows, matching the regression memo's independently measured figures.
`natural/z{6,8,10}` (289 tiles) is carried along as an anchor against the campaign's own tables.

**Arms — a single-variable A/B, not a proxy.** Both engines are constructed with *identical*
`EngineConfig`, including `compute_threads = default` and their own pool. The only difference is
the per-`Engine` threshold override (`Engine::set_serial_fallback_max_rows_for_test`,
`bench-timing`-gated, `crates/tessera-engine/src/session.rs:569`): `u64::MAX` forces the serial
fold on every request, `0` forces the `pool.install` fan-out on every request. Each row is
therefore a direct A/B of the exact branch at `crates/tessera-engine/src/viewport.rs:754-770`.

This differs from `calibration_sweep.rs`, whose "serial" arm is `compute_threads = 1` — still
inside `pool.install`, still paying scheduling overhead the real fallback does not (that tool's own
module doc says so and calls its crossover conservative). Ratios here are consequently **not**
directly comparable with the campaign's; they measure the shipped branch instead of a proxy for it.

**Reps** are interleaved (serial, parallel, serial, parallel, …), 25 per arm at 2.42M/1e8 and 15 at
1e9, after 3 warm-up reps per arm per shape. Medians reported. Every run went through
`scripts/bench-slot.sh` on a quiet box (12-core WSL2, 47 GiB); no build ran during any measurement.

**Fixtures:** `data/bench-fixtures/{1e8,1e9}` rebuilt for this work (the campaign's were deleted);
2.42M is the existing `/tmp/tessera-2m4`. The 1e9 rebuild produced 47,013,964,116 bytes against the
campaign's 47,017,049,354 — 0.007% apart, i.e. the same corpus.

| scale | items | terms | pairs | bundle bytes | build wall |
|---|---:|---:|---:|---:|---:|
| 1e8 | 100,000,000 | 4,930 | 171,720,580 | 4,699,710,586 | 1:02.1 |
| 1e9 | 1,000,000,000 | 47,968 | 1,718,472,823 | 47,013,964,116 | 10:28.8 |

Both `categories-subclass`, `--limit` prefix of the shared `data/scaled/geometry.parquet`, extent
`0,65536,0,65536`, `--mint-external-ids`, key `000102…0f`, epoch 1 — the same invocation
`bench-1e9-report.md` §1 records.

## Files

| file | what |
|---|---|
| `raw/tiles-{2m4,1e8,1e9}-{dense,sparse}.txt` | the six sweeps, 35 shapes each; `CSV,` lines are machine-readable |
| `raw/criterion-viewport-stock.txt` | `cargo bench -p tessera-engine --bench viewport` at this commit, stock 500M constant |
| `analyse.py` | rule evaluation over every cell; regret is arithmetic on measured medians |
| `analysis.txt` | its output |

`dense` is the bench's own `GrantShape::Random` w=10 grant (the realistic one, per the campaign's
fix round 1); `sparse` is the campaign's original deterministic even-spacing grant. Both are run at
every scale because mask density is the quantity the predictor deliberately does not model.

## The measured crossover, all six runs

Last tile count that measured SERIAL-favouring → first that measured PAR-favouring, per family.
**Measured.**

| family | rows spanned | 2.42M dense | 2.42M sparse | 1e8 dense | 1e8 sparse | 1e9 dense | 1e9 sparse |
|---|---|---|---|---|---|---|---|
| `natural` (81–289 tiles) | 0 – 83 M | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — | 289 → — |
| `f01` | 0 – 3.5 M | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 | 4225 → 16641 |
| `f04` | 0 – 10.2 M | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 | 2116 → 8281 |
| `f12` | 43 – 55 M | 4225 → 16641 | 4225 → 16641 | 1089 → 4225 | 4225 → 16641 | 1089 → 4225 | 4225 → 16641 |
| `f35` | 242 K – 131 M | 2116 → 8281 | 529 → 2116 | 2116 → 8281 | 529 → 2116 | 2116 → 8281 | 529 → 2116 |
| `f100` (whole corpus) | 2.4 M – 1e9 | PAR from 16 | PAR from 16 | PAR from 16 † | PAR from 16 | PAR from 16 | PAR from 16 |

† `f100` at 1e8 dense is the one non-monotone family: `z2` 0.87 PAR, `z3` 1.13 and `z4` 1.07
SERIAL, `z5`+ PAR. All three are within 13% of parity.

**Over all 210 cells: the highest tile count that measured serial-favouring is 4,225; nothing at or
above 8,281 tiles measured serial-favouring anywhere, at any scale, under either grant.** The
conflict band on the tile axis is therefore `[2116, 4225]` — three orders of magnitude narrower
than the row axis's `[242 K, 354.9 M]`.

## Rule evaluation, all 210 cells

*Regret* = how much slower the arm a rule picks is than the better of the two **measured** arms for
that same shape. It is arithmetic over measured medians, not a model.

| rule (parallel iff …) | total regret | cells misclassified | worst ratio | worst absolute |
|---|---:|---:|---:|---:|
| always serial (pre-parallel path) | 302.09 ms | 87 | 4.09× | 39.21 ms |
| always parallel | 66.81 ms | 123 | **62.35×** | 1.20 ms |
| `rows ≥ 500M` — **status quo** | 244.47 ms | 73 | 4.09× | 39.21 ms |
| `rows ≥ 500M OR tiles ≥ 1024` | 20.91 ms | 41 | 9.23× | 1.75 ms |
| `rows ≥ 500M OR tiles ≥ 2048` | 20.37 ms | 33 | 3.81× | 3.15 ms |
| **`rows ≥ 500M OR tiles ≥ 4096`** | **18.38 ms** | **27** | **2.81×** | 3.15 ms |
| `rows ≥ 500M OR tiles ≥ 8192` | 34.55 ms | 23 | 3.47× | 7.63 ms |
| `rows ≥ 500M OR tiles ≥ 16384` | 44.13 ms | 35 | 3.47× | 7.63 ms |
| `tiles ≥ 4096` alone (no row term) | 28.68 ms | 35 | 2.81× | 3.25 ms |
| oracle (per-cell best) | 0 | 0 | — | — |

Reproduce: `python3 analyse.py raw/*.txt`.

## Why the tile axis is the stable one

Serial cost per tile, over the cells with ≥ 1,000 tiles (**measured**, from `analysis.txt`):

| scale/grant | min ns/tile | median | max |
|---|---:|---:|---:|
| 2.42M dense | 85.0 | 112.8 | 2,970 |
| 2.42M sparse | 87.1 | 122.8 | 4,769 |
| 1e8 dense | 107.4 | 188.1 | 1,846 |
| 1e8 sparse | 99.3 | 155.5 | 4,753 |
| 1e9 dense | 140.6 | 486.6 | 4,486 |
| 1e9 sparse | 136.3 | 328.2 | 7,124 |

The **floor** — the cost of a tile that resolves to little or no work — moves only 85 → 141 ns
across a 400× change in corpus size and both grants. That floor is per-tile fixed cost (the range
setup, the `count_range` entry, the probe) and it is mask-independent by construction. The maxima
are the `f100` cells where each tile spans a large slice of the corpus, i.e. where the *row* term
is doing the work.

Against that, the row axis's coefficient is not stable at all: `f100/z2` at 2.42M costs 1.11 ms for
2,422,486 rows (**0.46 ns/row**), while `natural/z4/s6` at 1e9 in the campaign's own data costs
669 µs for 354,900,645 rows (**0.0019 ns/row**) — a 240× spread in the same coefficient
(`../2026-07-31-concurrency-workstream/bench-runs/recalibration/calib-1e9-dense.txt:12`). That is
B9's tiered decode: after it, rows-in-range stopped measuring work, exactly as the calibration
memo's result 1 says. Tile count did not.

## The 1e9 natural regression is not reachable by any tile arm

The row figure the 500M constant was sized above — 354,900,645 — belongs to `natural/z4/s6`, and
that shape resolves **81 tiles**
(`../2026-07-31-concurrency-workstream/bench-runs/recalibration/calib-1e9-dense.txt:12`, the
campaign's own raw output). The whole `natural` family is 81 tiles at z4 and 289 at every zoom from
5 up, at every scale, because its span formula (`extent / 2^(zoom−4)`) is exactly 16 cells wide at
every depth. A tile threshold of 4,096 is 14–50× above it. Confirmed independently here: all
eighteen `natural` cells measured serial-favouring (ratios 2.81–34.04), and none would change arm
under any tile threshold ≥ 1,024.
