# An entity-ordered text arena, and the scatter that turned out to matter more

⊘ **2026-09-04, dated note.** `--arena-order` and the entity-ordered two-pass fill this probe built
are deleted (owner ruling, `build-prose-extents.md` §6): the ascending scatter this probe found —
§3 below — is what stays. The measurements below are unaffected history.

**Date** 2026-09-03. **Branch** `build/entity-ordered-arena`. **Box** WSL2, 12 cores, 47 GB, local
NVMe-backed VHDX. **Corpora** `$TESSERA_LADDER/medcpt-1m` and `medcpt-10m-abs` — 10,000,000
articles, 6,893,387 of them with an abstract, a 10.2 GiB string payload across four declared
string columns. Every run is `tessera build --stage-timings` under `sample_rss.py`.

⊘ **The box was quiet for every measured run**: no other `tessera build`, no serve battery. The
uncapped and capped pairs are each two runs back to back, which
[`../2026-09-03-text-arena-streaming/`](../2026-09-03-text-arena-streaming/README.md) §6 gives the
reason for.

## The finding

**Two things were random about the attribute join, and the cheaper one was doing most of the
damage.** The owner ruling asked for an entity-ordered arena, filled by a second decode of the
source's text column when the arena would not fit the page cache, so that `record_blob`'s
entity-order walk over it becomes sequential. That is built and it works. But building it exposed
that the join's **scatter** — the write of each resolved row into its entity-order column — was
itself a uniformly random walk, and fixing *that* is what makes the record blob finish under a cap:

| 10⁷ under `MemoryMax=4G`, `record_blob` | arrival arena | entity arena |
|---|---|---|
| scatter in the sweep's own order | ⊘ **> 2,134 s, killed unfinished** — 85.5 MB of a 4.4 GB `blocks.bin`, 91.7 major faults/s, **2,348 GiB read** for a 10.2 GiB arena | — |
| **scatter ascending in the entity** | **64.31 s** | **60.72 s** |

The uncapped wall is 64.69 s and 76.00 s. **The blob finishes at its uncapped wall in both orders,
once the scatter ascends.** §3.

**The two orders are byte-identical**, at 10⁶ and 10⁷, against each other and against `main`. §4.

**The second decode costs what a second decode costs: 2.04× the join**, 32.45 s → 66.18 s uncapped
at 10⁷. Under the cap it is 46.46 s → 131.75 s. §2.

⊘ **Rung 4 was not built: the disk pre-flight refuses it.** 274.5 GB modelled need against 257.7 GB
free, short by 16.3 GiB, with **209 GB of the volume held by another session's
`.claude/worktrees/agent-a791111a727ec646b/target/debug`**. Nothing of this change is implicated and
nothing measured here is in doubt; the box simply has no room. §5.

## 1. The switch

`--arena-order auto|entity|arrival`, default `auto`. **`auto` takes `entity` when the sum of the
declared string columns' uncompressed Parquet payload exceeds half the memory budget**, both read
before the join — the payload from the source footers (`residency::ColumnCost::payload_bytes`), the
budget from `--memory-budget` or the detector. Half, because the arena is not alone in the page
cache at the join and is not alone in it at the stages that read it; `residency::ARENA_BUDGET_SHARE`
carries the argument and the ⊘ that it is modelled rather than swept.

What it printed at 10⁷, uncapped:

```
  string column 'title': 919 MiB of characters (uncompressed, from the footer)
  string column 'mesh_major': 525 MiB of characters (uncompressed, from the footer)
  string column 'abstract': 8755 MiB of characters (uncompressed, from the footer)
  string column 'pmid': 113 MiB of characters (uncompressed, from the footer)
  arena: 10313 MiB of string payload against a 35565 MiB budget, share 17782 MiB (half) — arrival
```

and under `MemoryMax=4G`, where the same corpus flips:

```
  arena: 10313 MiB of string payload against a 3276 MiB budget, share 1638 MiB (half) — entity
```

⊘ **At 10⁸ the line was never reached**, because the disk pre-flight refuses before the join (§5).
The payload it would have summed is in that refusal's own breakdown, from the same footers:
**113,576 MiB of abstracts, 8,836 MiB of titles and 1,456 MiB of keyword — 123,868 MiB**. The
budget is `MemAvailable` damped, which this box derived at **27,173 and 35,565 MiB** on the two runs
above, so the share is 13,500–17,800 MiB and `auto` takes `entity` by a factor of seven to nine.
⊘ Derived from the refusal's own figures, not read off the line the switch would have printed.

## 2. What the second decode costs

Uncapped, 10⁷, back to back:

| stage | `arrival` | `entity` |
|---|---|---|
| `attribute_tail` | **32.45 s** | **66.18 s** (2.04×) |
| `layers` | 101.72 s | 111.24 s |
| `text_index` | 103.50 s | 96.97 s |
| `filter_postings` | 24.79 s | 33.40 s |
| `record_blob` | 64.69 s | 76.00 s |
| whole run | **395.03 s** | **450.99 s** (+14.2%) |

**The price is the decode and nothing else.** Pass one keeps a `u32` length per entity and writes no
prose; the prefix sum is one forward sweep over the presence bits; pass two projects only the string
columns. What is left is reading and decoding the source's prose twice, and it doubles the join.

## 3. The scatter, which is the finding

**The join's answer arrives in source-id order and entity ids are signature-then-Morton order**, so
each lane wrote its column at a uniformly random index — the same unrelatedness that
[`../2026-09-03-text-arena-streaming/`](../2026-09-03-text-arena-streaming/README.md) §2 measured
between entity order and arrival order, one structure earlier. It was invisible while every column
fitted the page cache, and it is not invisible for a lane that writes a whole 1.2 kB record.

Measured at 10⁷ under `MemoryMax=4G`, before the fix:

| | `entity`, pass two | `arrival`, `record_blob` |
|---|---|---|
| wall | ⊘ **1,335 s into `attribute_tail`, unfinished** | ⊘ **> 2,134 s, unfinished** |
| against | 46.46 s | 64.69 s uncapped |
| major faults | **347,009** | 195,676 (**91.7/s**) |
| `read_bytes` | **480 GB** — 45× a 10.2 GiB arena | **2,348 GiB** — 230× |
| PSI io `full` | — | up to **69.0%** |
| written | — | **85.5 MB** of a 4.4 GB `blocks.bin` |

**One stable sort of the chunk's resolved rows by entity fixes both.** Each lane becomes a forward
sweep of its column, and there are `n / staging_rows` of them per source — six on this corpus. It
costs a sort of at most `staging_rows` `(u32, u32)` pairs a chunk and it is what the whole-run
figures in §2 and §4 are measured with.

**It is stable because pass one and pass two must agree about which of two rows carrying one entity
is written last** — that is the length pass one reserved. `join_chunk`'s own sort was made stable
for the same reason; it was `par_sort_unstable_by_key`, which left that question answered
differently on two runs of the same build.

⊘ **Six sweeps is a property of this corpus, not of the change.** `staging_rows` is
`JOIN_STAGE_BYTES / per_row` and does not grow with `n`, so at 1.02×10⁸ it would be about 54, and 54
concurrent forward streams over a 128 GiB arena is a page-cache question this probe did not get to
ask (§5). **That is the term on which the entity-ordered arena — one stream, not 54 — is still
expected to pay, and it is the one thing here that is argued rather than measured.**

## 4. The bytes

`medcpt-1m` and `medcpt-10m-abs`, each built three ways — `main` at `ee699098`, this branch with
`--arena-order arrival`, and with `--arena-order entity` — and compared file by file:

| | files | differing |
|---|---|---|
| `medcpt-1m`, all three pairs | 36 | **0**, bar `MANIFEST.json`'s `created_at` and the `CURRENT` digest that follows it |
| `medcpt-10m-abs`, all three pairs | 38 | **0**, same two |

The arena is `.build-tmp/` scratch and its order reaches no artefact, which is the whole argument for
letting a switch choose between the two fills. `tests/text_index.rs`'s
`the_two_arena_orders_build_the_same_bundle` asserts it on a fixture so the property is checked
without a corpus.

⊘ **The capped pair's bundles were not compared** — the harness deleted the arrival one before the
comparison. The uncapped three-way above is the evidence, and the capped runs produced the same
`bundle_bytes` (7,705,530,732) and the same term and pair counts.

## 5. Rung 4, refused

```
build FAILED: invalid input: insufficient disk for this build: ~274545822083 bytes needed at peak
(spill phase 612704058, band phase 1021173430, column phase 274545822083, assembly phase
3688989016; n = 102117343, pairs = 102117343, batches = 1), 257664073728 available at the output
path
```

**16.3 GiB short.** The column phase is the refusal, and two terms are 84% of it: the abstract
column at **115,536 MiB** and the text index's sorted runs *charged at the column they are tokenised
from* at **115,147 MiB**. ⊘ That second term is modelled and known loose — `residency.rs` records
7.4×10⁷ Overture names spilling 555 MB against the 2.58 GB it charges — so the real need is
materially below 274.5 GB. **It was not loosened to make this build fit**: the pre-flight is there
to pre-empt an ENOSPC halfway through a multi-hour build, and moving a fail-closed threshold to get
a measurement is the wrong trade.

**What is actually holding the volume is not the corpus.** 209 GB of the 1,007 GB filesystem is one
`target/debug` under `.claude/worktrees/agent-a791111a727ec646b/`, last written 2026-09-03 09:58.
Everything this probe owned was freed — its own bundles, its `target/debug`, and the reference
worktree — for 1.4 GB. **Reclaiming that directory is what unblocks rung 4**, and it is another
session's, so this probe did not touch it.

⊘ **So the rung 4 questions the brief asked are still open**: whether the 10⁸ build completes, its
stage walls, its bundle size and breakdown, `verify --deep`, and the serve battery. Nothing measured
here suggests it would not; the 54-stream question in §3 is what it would settle.

## 6. What is here

| | |
|---|---|
| the samplers | [`../2026-09-03-text-arena-streaming/sample_rss.py`](../2026-09-03-text-arena-streaming/sample_rss.py) and `compare_bundles.py` beside it, used unchanged rather than copied |
| `uncapped-{arrival,entity}.{rss,stages}.csv` | §2's pair |
| `cap4g-{arrival,entity}.{rss,stages}.csv` | §3's pair, with the scatter sorted |
| `rung4.log` | §5's refusal, with the full residency breakdown. No RSS trace: the build never started |

⊘ The pre-fix figures in §3's table are quoted from the runs that motivated the sort and their CSVs
were overwritten by the post-fix pair; the numbers are in the commit message of `0f59cadb` and in
this file, and the shape is reproducible by reverting that one sort.
