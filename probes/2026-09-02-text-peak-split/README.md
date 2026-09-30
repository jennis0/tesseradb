# Splitting the build's peak: anonymous memory against page cache, on abstracts

**Date** 2026-09-02. **Binary** `850dab8f`, `cargo build --release -p tessera-cli`, run from the
worktree's own `target/`. **Box** WSL2, 12 cores, 47 GB, local NVMe, ~260 GB free; ⊘ another
session's server and two other release builds were live for part of the campaign — the load is
recorded per run in §6 and no measurement here was taken alone on the box.

## The finding

**The prose is page cache, not memory.** At 10⁷ rows the abstract column costs **+9,594 MiB of
`VmHWM`** and **+562 MiB of anonymous memory**: 94% of what the build reports as its peak is
file-backed pages the kernel may evict. Under `--memory-budget 6g` the anonymous cost falls to
**+208 MiB** — the text pass spills 298 runs instead of 96 and the cascade fires — while `VmHWM`
barely moves, which is the signature of a bound that binds.

So `test_corpora/medcpt/README.md`'s ~81 GB at 36M, and the campaign plan's "streaming text
column", both read the wrong number. Extrapolated **anonymous** memory per row from 10⁷:

| | 3.592×10⁷ (rung 3, whole) | 1.02×10⁸ (rung 4) |
|---|---|---|
| no abstracts | 16.4 GB | 46.7 GB |
| abstracts, auto budget | 18.6 GB | 52.7 GB |
| abstracts, `--memory-budget 6g` | 17.2 GB | 48.9 GB |
| *the abstracts' own share, auto / 6g* | 2.1 / 0.8 GB | 6.0 / 2.2 GB |

**Modelled, not measured** — linear in rows from one point. The no-abstracts row is the one with a
check: rung 3's real whole-corpus build measured **16.03 GB** of `VmHWM` (`docs/ingest-campaign.md`
§4.6) against 16.4 GB predicted here, 2% apart, and without abstracts `VmHWM` and the anonymous
figure are nearly the same number.

**What the extrapolation is dominated by is not the prose.** Every row above is mostly the
`manifests` stage — the artifact layout over this corpus's MeSH DAG, 471,778,374 member rows at
10⁷ — which is what `residency.rs` and the campaign's W2 already name. Abstracts move it by 2 GB
at 36M. ⊘ **The 1.02×10⁸ column does not transfer to rung 4 on its own**: that number is this
corpus's hierarchy scaled up, and rung 4's layers are not MedCPT's. What does transfer is the
abstracts' own share, because that is a property of the prose.

**The machinery the plan called for already exists.** The entity-order columns are mapped
(`column.rs`), the text index spills sorted runs under a budget and cascades them
(`pipeline.rs`, `TextIndexPlan`), and the record blob streams. There is nothing to build for
rung 4's abstracts; what is left is a disk question and a wall-clock question, not a memory one.

## 1. What was measured, and why `VmHWM` could not answer it

`tessera build --stage-timings` prints one figure a stage, `peak_rss_kib` from `/proc/self/status`'s
`VmHWM` (`observer.rs`). `VmHWM` is the high-water of *resident* pages and does not distinguish

- **`RssAnon`** — heap and stacks. The kernel must find it somewhere: RAM or swap. This is what an
  OOM kill is about and what `--memory-budget` models.
- **`RssFile` + `RssShmem`** — pages of mapped files. Since 2026-08-30 the entity-order columns and
  the render tail are `MappedArray`s under `.build-tmp/` (`residency.rs`'s "what moved off the heap,
  and is still counted"), the text index's runs are spill files, and the record blob streams. On a
  corpus of prose this is most of the resident set, and the kernel evicts it under pressure.

`sample_rss.py` polls all five fields every 100 ms and timestamps the child's stderr on the same
clock, so a sample can be attributed to the stage that produced it.

```
sample_rss.py --out PREFIX -- tessera build --deployment ./tessera.toml --out ./bundle --stage-timings
sample_rss.py --out PREFIX --report
```

⊘ **A stage shorter than 100 ms gets the nearest single sample**, marked `*` in the tables below and
in the tool's own output. Nothing is interpolated. At 10⁷ only the two zero-second stages are
affected; at 10⁶ most of the pre-attribute stages are, which is why the 10⁶ tables are read for
their tail and not their head.

## 2. The corpora

`~/venvs/projection/bin/python -m test_corpora.medcpt.prepare --sample N [--abstracts] --out …`,
from `$TESSERA_LADDER/medcpt/staging/`.

| | rows | abstracts | `points.parquet` |
|---|---|---|---|
| `medcpt-1m` | 1,000,000 | — | 118.6 MB |
| `medcpt-1m-abs` | 1,000,000 | 689,132 (68.9%) | 634.6 MB |
| `medcpt-10m-abs` | 10,000,000 | 6,893,387 (68.9%) | 6,240.7 MB |

**The 10⁷ control is the same corpus with the column undeclared**, not a second `prepare.py` run:
`corpus-noabs.toml` beside `corpus.toml` in `medcpt-10m-abs/`, identical but for the `abstract`
`[[attribute]]` block, run as `tessera build --config ./corpus-noabs.toml`. The rows, the layout,
the k-means and the 471,778,374 MeSH member rows are then bit-identical between the two arms, so
every difference below is the abstract column and nothing else. This is a stronger control than the
10⁶ pair, whose two `prepare.py` runs sampled the same seed but wrote separate files.

## 3. The 10⁶ pair — the README's figures, decomposed

Auto budget both. Anonymous and file-backed columns are each that stage's own high-water over its
samples; `VmHWM` is what the build printed.

**Without abstracts** (`1m-noabs.rss.csv`) — 19.77 s, bundle 332,688,528 B:

| stage | s | anon MiB | file MiB | VmHWM MiB |
|---|---|---|---|---|
| source_ids | 0.04 | 13 | 12 | 32 |
| dictionary | 0.69 | 101 | 13 | 130 |
| geometry_read | 0.48 | 188 | 13 | 235 |
| pairs_pack | 0.00 | 91 * | 28 * | 235 |
| signature_sort | 0.13 | 144 | 28 | 235 |
| assignment | 0.27 | 131 | 28 | 235 |
| assignment | 0.03 | 66 | 21 | 235 |
| postings_write | 0.13 | 84 | 22 | 235 |
| external_ids | 0.00 | 84 * | 22 * | 235 |
| attribute_tail | 0.50 | 102 | 279 | 393 |
| layers | 10.36 | 545 | 105 | 648 |
| text_index | 1.48 | 421 | 210 | 648 |
| filter_postings | 0.89 | 421 * | 210 * | 648 |
| record_blob | 1.00 | 421 | 210 | 648 |
| column_release | 0.02 | 421 * | 210 * | 648 |
| tiler_sort | 0.02 | 421 * | 40 * | 648 |
| segment_write | 0.07 | 421 | 40 | 648 |
| **manifests** | 3.61 | **629** | 40 | **728** |
| whole run | 19.77 | **629** | **279** | **729** |

**With abstracts** (`1m-abs.rss.csv`) — 34.11 s, bundle 798,397,569 B:

| stage | s | anon MiB | file MiB | VmHWM MiB |
|---|---|---|---|---|
| source_ids | 0.03 | 8 | 12 | 31 |
| dictionary | 0.69 | 104 | 13 | 124 |
| geometry_read | 0.51 | 186 | 13 | 233 |
| pairs_pack | 0.00 | 90 * | 28 * | 233 |
| signature_sort | 0.13 | 147 | 28 | 233 |
| assignment | 0.22 | 134 | 28 | 233 |
| assignment | 0.03 | 78 * | 21 * | 233 |
| postings_write | 0.13 | 78 | 21 | 233 |
| external_ids | 0.00 | 68 * | 22 * | 233 |
| **attribute_tail** | 3.11 | 206 | **1,505** | 1,646 |
| layers | 10.67 | 539 | 479 | 1,646 |
| **text_index** | 8.46 | 927 | 1,194 | **2,141** |
| filter_postings | 0.81 | 927 * | 1,095 * | 2,141 |
| record_blob | 5.40 | 927 | 1,095 | 2,141 |
| column_release | 0.05 | 927 * | 33 * | 2,141 |
| tiler_sort | 0.03 | 927 | 33 | 2,141 |
| segment_write | 0.07 | 927 * | 41 * | 2,141 |
| **manifests** | 3.67 | **1,145** | 41 | 2,141 |
| whole run | 34.11 | **1,145** | **1,505** | **2,141** |

**The README's pair is reproduced.** Its wall times were 19.7 s and 34.3 s against 19.77 and 34.11
here; its peaks were 716 MB and 2,246 MB against 728 and 2,141 MiB; its bundles 333 MB and 799 MB
against 332.7 and 798.4 MB. The peaks differ by 2% and 5% — a different output path, a different
day's load, and the build's own figure is MiB where the README wrote MB.

**Decomposed**: abstracts cost **+516 MiB of anonymous memory** and **+1,226 MiB of page cache**.
The stage that owns the anonymous high-water is `manifests` in *both* arms.

## 4. The 10⁷ triple — the real test

**Wall time, bundle, and the two peaks:**

| 10⁷ rows | wall | bundle bytes | anon MiB | file MiB | VmHWM MiB |
|---|---|---|---|---|---|
| no abstracts (control) | 199.28 s | 3,163,528,625 | 4,366 | 1,975 | 4,692 |
| abstracts, auto budget | 346.01 s | 7,705,530,732 | 4,928 | 11,955 | 14,286 |
| abstracts, `--memory-budget 6g` | 345.18 s | 7,705,530,732 | 4,574 | 11,839 | 13,904 |
| **abstracts' cost, auto** | +146.7 s | +4,542,002,107 | **+562** | +9,980 | +9,594 |
| **abstracts' cost, 6g** | +145.9 s | +4,542,002,107 | **+208** | +9,864 | +9,212 |

**Per stage** (`10m-noabs.rss.csv`, `10m-abs-auto.rss.csv`, `10m-abs-6g.rss.csv`), anonymous MiB
first, file-backed MiB second:

| stage | control | abstracts, auto | abstracts, 6g |
|---|---|---|---|
| source_ids | 103 / 12 | 81 / 13 | 86 / 12 |
| dictionary | 832 / 13 | 819 / 13 | 819 / 13 |
| geometry_read | 1,885 / 69 | 1,861 / 143 | 1,895 / 79 |
| pairs_pack | 636 / 165 * | 618 / 143 * | 618 / 165 * |
| signature_sort | 1,191 / 165 | 1,142 / 166 | 1,151 / 165 |
| assignment | 943 / 165 | 878 / 166 | 900 / 165 |
| assignment | 401 / 89 | 336 / 140 | 358 / 89 |
| postings_write | 566 / 90 | 514 / 90 | 535 / 90 |
| external_ids | 351 / 90 * | 363 / 90 * | 535 / 90 * |
| **attribute_tail** | 422 / **1,431** | 527 / **7,864** | 532 / **7,862** |
| layers | 1,644 / 863 | 1,638 / 1,483 | 1,638 / 1,482 |
| **text_index** | 1,762 / 1,975 | **2,379** / **11,955** | **1,946** / 11,839 |
| filter_postings | 1,762 / 1,975 * | 2,327 / 10,762 * | 1,946 / 10,784 * |
| record_blob | 1,762 / 1,949 | 2,327 / 10,763 | 1,946 / 10,762 |
| column_release | 1,762 / 171 * | 2,327 / 9,760 | 1,946 / 10,658 |
| tiler_sort | 1,765 / 247 | 2,327 / 244 | 1,946 / 247 |
| segment_write | 1,765 / 324 | 2,287 / 325 | 1,946 / 325 |
| **manifests** | **4,366** / 247 | **4,928** / 248 | **4,574** / 248 |

And the stage wall times, where the abstracts are the whole of the difference:

| stage | control | abstracts, auto | abstracts, 6g |
|---|---|---|---|
| attribute_tail | 4.40 s | 34.79 s | 31.56 s |
| layers | 100.44 s | 104.91 s | 99.89 s |
| text_index | 11.45 s | 68.89 s | 80.25 s |
| record_blob | 10.10 s | 58.29 s | 54.11 s |
| manifests | 36.84 s | 40.07 s | 41.19 s |

### 4.1 Where the anonymous memory is, and where it is not

**The anonymous high-water is `manifests` in all three arms** — the artifact layout and the
containment partitions over 471,778,374 MeSH member rows — and it moves by 562 MiB when 6.9 million
abstracts are added to a corpus whose rows, layout and hierarchy are otherwise identical. The prose
is not what the build must hold.

**`attribute_tail` is the clearest case of the split doing its job.** Adding the column moves its
file-backed high-water from 1,431 to 7,864 MiB and its anonymous one from 422 to 527. That is the
mapped-arena construction in `column.rs` behaving exactly as its module doc says: the bytes are page
cache, and the 24-byte `String` header per entity the doc measured on Overture is not there to be
counted.

**`text_index` is where the budget shows.** Its anonymous figure is 1,762 MiB in the control — which
is the `layers` stage's 1,644 MiB carried forward by the allocator, not the text pass — and 2,379
under the auto budget against 1,946 under 6g. So the abstract column's own anonymous cost in the
pass that indexes it is **~617 MiB at the auto budget and ~184 MiB at 6g**, and the flag moves it.
The 6g arm's text stage is 11.4 s slower for it.

### 4.2 The spill runs, and the cascade

Counted by polling `<out>/v00000/partitions/default/attrs/<col>/` for `text-run-*.spill` and
`text-merge-*.spill` every 2 s while the build ran (a run is deleted only at the cascade or at the
merge, so a 2 s poll cannot miss one). The pass takes `threads × 8 = 96` chunks at 10⁷ rows.

| run | chunks | abstract | title | mesh_major | cascade |
|---|---|---|---|---|---|
| 10⁶, auto | 16 | 16 | 8 ⊘ | 12 ⊘ | — |
| 10⁷, auto | 96 | 96 | 96 | 83 | — |
| 10⁷, `--memory-budget 6g` | 96 | **298** | 96 | 83 | **fired**: one pass, 298 → 3 |

⊘ **The 10⁶ row's second and third columns are undercounts**, not findings: at 10⁶ the chunk count
is the 65,536-entity floor rather than the thread rule, and the text stage runs for 1.5–8.5 s, which
a 2 s poll does not sample cleanly. Every column there is one run a chunk; the abstract column's 16
is the one the poll caught whole.

**The 6g arm is the one the design predicted.** `TEXT_BUDGET_SHARE` is a sixteenth, so 6 GiB gives
the whole pass 384 MiB and each of twelve workers 32 MiB; the abstract column overflows a worker
roughly three times a chunk, 298 runs exceed `TEXT_MERGE_FAN_IN` (128), and `cascade_text_runs`
merges them in one pass into 3 intermediate runs before the final merge. Title and MeSH are
unaffected — 96 and 83 runs in both arms — because neither fills even a 32 MiB accumulator.

**The bundle is byte-identical across the two budgets** (7,705,530,732 both), which is what
`chunking_the_text_index_does_not_change_its_bytes` asserts in the small and this confirms at 10⁷:
the run count is a memory knob, not a format decision.

## 5. Sizing rung 4's bundle

Per-row from the 10⁷ measurement, linear — **modelled**:

| | 10⁷ measured | B/row | 3.592×10⁷ | 1.02×10⁸ |
|---|---|---|---|---|
| bundle, no abstracts | 3,163,528,625 | 316.4 | 11.4 GB | 32.3 GB |
| bundle, abstracts | 7,705,530,732 | 770.6 | 27.7 GB | 78.6 GB |
| `attrs/abstract/` (dict + postings) | 1,365,054,851 | 136.5 | 4.9 GB | 13.9 GB |
| — of which `dict.bin` | 17,477,969 | 1.7 | 0.06 GB | 0.18 GB |
| — of which `postings.arrow` | 1,347,576,882 | 134.8 | 4.8 GB | 13.8 GB |
| record blob `blocks.bin`, abstracts | 3,716,028,194 | 371.6 | 13.3 GB | 37.9 GB |
| record blob `blocks.bin`, control | 540,228,026 | 54.0 | 1.9 GB | 5.5 GB |

The 3.592×10⁷ bundle column is a second check: 27.7 GB against the medcpt README's independently
modelled 28.7 GB.

**The record blob is the larger half of the abstracts' cost on disk, not the index.** Storing the
prose costs 3.18 GB per 10⁷ rows against the index's 1.37 GB, and at 1.02×10⁸ the split is 32.4 GB
against 13.9 GB. A rung that wanted the abstracts searchable but not retrievable would pay a
quarter of this; that is a corpus declaration, not a build change.

**File-backed pages, for the disk pre-flight rather than for memory**: 11,955 MiB resident at 10⁷,
1,253.6 B/row, so 45.0 GB at 3.592×10⁷ and 127.9 GB at 1.02×10⁸ of mapped file the build *touches*.
It is not what the build needs to hold — the kernel evicts it — and it is not the disk figure
either, which is `.build-tmp/` plus the bundle and is what `residency.rs`'s `mapped` half and the
disk pre-flight already report.

## 6. Method, load and reproduction

```bash
export TESSERA_LADDER=/home/user/code/tessera/data/ladder
cargo build --release -p tessera-cli                                   # in this worktree

~/venvs/projection/bin/python -m test_corpora.medcpt.prepare \
    --sample 1000000  --abstracts --out $TESSERA_LADDER/medcpt-1m-abs
~/venvs/projection/bin/python -m test_corpora.medcpt.prepare \
    --sample 10000000 --abstracts --out $TESSERA_LADDER/medcpt-10m-abs

cd $TESSERA_LADDER/medcpt-10m-abs && set -a && . ./.env && set +a
python3 probes/2026-09-02-text-peak-split/sample_rss.py --out …/10m-abs-auto -- \
    …/tessera build --deployment ./tessera.toml --out ./bundle-auto --stage-timings
python3 probes/2026-09-02-text-peak-split/sample_rss.py --out …/10m-abs-6g -- \
    …/tessera build --deployment ./tessera.toml --out ./bundle-6g --stage-timings --memory-budget 6g
python3 probes/2026-09-02-text-peak-split/sample_rss.py --out …/10m-noabs -- \
    …/tessera build --deployment ./tessera.toml --out ./bundle-noabs --stage-timings \
                    --config ./corpus-noabs.toml
```

Raw CSVs beside this file: `<tag>.rss.csv` (t, RssAnon, RssFile, RssShmem, VmRSS, VmHWM in KiB) and
`<tag>.stages.csv` (t, the build's own stderr line) for `1m-noabs`, `1m-abs`, `10m-noabs`,
`10m-abs-auto` and `10m-abs-6g`. Every table above is `sample_rss.py --report` over them.

⊘ **The box was not idle.** Every run in §3 and §4 shares the machine with a MedCPT server holding
~11 GB and, for part of the evening, other sessions' `cargo build`s. Anonymous high-waters are the
build's own `/proc` figures and are unaffected by a neighbour; the *file-backed* ones are page cache
and a busier box would show fewer of them resident, so those are an upper bound on what this build
got, not on what it wants. Wall times are indicative for the same reason, and the 10⁶ pair
reproducing the medcpt README's 19.7 / 34.3 s to within 1% suggests the interference was small.

⊘ **A first pass of every measurement was taken at `af62eca9`**, the branch's base, before the
branch was fast-forwarded to `850dab8f`; the tables here are the second pass, all five runs on one
binary. The two passes agree to within 2% on every anonymous high-water. Nothing between those
commits touches `pipeline.rs`, `column.rs`, `residency.rs` or `spill.rs`.

## 7. What this does not answer

- **Rung 4's own hierarchy.** The dominant anonymous term is `manifests` over MedCPT's MeSH DAG.
  A rung 4 with different layers has a different number there, and the 1.02×10⁸ column of the
  finding above is not a prediction for it.
- **Whether 1.02×10⁸ fits.** 46.7–52.7 GB of anonymous memory on a 47 GB box is not a comfortable
  margin, and the term that makes it uncomfortable is the hierarchy, which is W2's territory and
  `residency.rs`'s. Abstracts move that figure by 6 GB.
- **Whether the kernel's eviction is free.** These runs had 47 GB for a 12–14 GB resident set, so
  nothing was evicted and the file-backed half never had to be re-read. A build whose mapped
  columns exceed the box will page, and this probe says nothing about what that costs.
