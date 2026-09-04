# The layers stage's residency, and the corpora that can show it

Measured 2026-08-30, against `55b939cd`. What the three merged changes cost and bought at full
scale, and the two corpus shapes that had to be built before any of it could be seen.

## Result

Overture — 73,631,092 points, 5.07×10⁸ member pairs, warm page cache, builds back to back with
`attribute_tail` as the control:

| | control | `layers` | `layers` high-water | process max RSS |
|---|---|---|---|---|
| `16af78ed` the duplicate copy gone | 286.5 s | 165.9 s | 10,290 MiB | 10,698,504 kB |
| `b4dc03ca` spill and k-way merge | 279.7 s | 196.4 s | 7,780 MiB | 8,965,304 kB |
| `c3d422f9` packs a blob at a time | 520.7 s | 180.0 s | 6,933 MiB | 8,167,964 kB |

**10.20 → 7.79 GB, 23.6%.** Bundles byte-identical at full scale: only `CURRENT` and
`MANIFEST.json` differ, by `created_at` and the digest over it.

⊘ **The timings are comparable only within a batch.** `c3d422f9` was measured in an earlier batch
whose control ran at 520.7 s against this one's 286.5 s. Memory *is* comparable across both:
`b4dc03ca` was measured in each and came out 8,980,736 and 8,965,304 kB, 15 MB apart. Compare the
RSS column freely; compare the two time columns only against their own control.

⊘ **The first build of any batch is worthless.** Reading a 24 GB corpus cold put the control at
2052.8 s against 286.5 s warm — a 4× swing in a stage no change here touches, which read as a
spectacular result until the control was looked at. Discard a cold run; do not average it in.

## Two traps, both hit on the day

**GeoNames cannot show this change, and measures it at 75 MiB.** Its 6.84×10⁷ pairs are 547 MB at
8 B a pair — *smaller than the 1 GiB accumulator window* — so almost nothing spills and the
mechanism under test never runs. `b4dc03ca` was nearly abandoned on that number. A corpus whose
whole membership set fits in one window is not a small version of this problem; it is a different
one.

**A null `key` element is not an artifact.** `key` is a list per row, one element per ladder level,
and a null means the point is in no artifact at that level — unclustered, never spilled.
`pc.value_counts` ranks null as a value, and on `members-taxonomy.parquet` that bucket is 51.8% of
the entries. Read carelessly it says one artifact holds half the corpus; it does not, and no
artifact in Overture holds more than 8.0%. A day's work was aimed at a term that did not exist.
`pairs.py` here excludes nulls and reports them separately.

## The scripts

| | |
|---|---|
| `pairs.py PATH` | pairs per artifact for a `key`-list Parquet — nulls excluded and reported |
| `gen-dominant.py` | 2×10⁷ points, 2,049 artifacts, 4×10⁷ pairs, **one artifact holding half of them**. The only shape that exercises the largest-artifact term without a 24 GB corpus |
| `gen-shapes.py` | 2×10⁵ points: a nested tree, a tiered ladder, an attached label layer, a membership spelled by `excluding`. The byte-identity fixture |
| `measure.py BIN DEPLOYMENT OUT LOG [args…]` | runs a build, timestamps every line, samples RSS at 20 Hz, and reports max RssAnon/VmRSS **inside each stage's own window** |

`measure.py` is what `--stage-timings` is not: the build's own `peak=` is the process high-water at
the moment a stage ended, so it only rises and a stage that adds nothing repeats the last figure.
Per-stage *windows* are what attribute a term to the code that holds it.

## What is still there

Not fixed, and the next target: **`read_members` holds 1,318 MiB of the stage's ~1,800** at
GeoNames — three quarters of it, and untouched by anything merged here. Measured with a phase probe
that was not committed; re-instrument rather than trusting the figure.

The merge fan-in is unpinned. `cascade_member_runs` reduces above `MEMBER_MERGE_FAN_IN` = 128 runs;
at the `MEMBER_BUDGET_MIN` floor of 64 MiB that is 128 × 64 MiB = 8 GiB of pairs, i.e. **1.07×10⁹
pairs — 2.1× Overture, and reachable at `--memory-budget 1g`**. It is a live guard on the
constrained path, not speculative machinery; nothing pins that arithmetic to the two constants, so
a drift in either goes stale silently.
