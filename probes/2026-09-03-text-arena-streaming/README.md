# The text index over an arena larger than memory

**Date** 2026-09-03. **Branch** `build/text-arena-streaming`. **Box** WSL2, 12 cores, 47 GB, local
NVMe-backed VHDX. **Corpus** `$TESSERA_LADDER/medcpt-10m-abs` — 10,000,000 articles, 6,893,387 of
them with an abstract, a 6.24 GB `points.parquet` and a **13.2 GB** `abstract` arena. Every run is
`tessera build --stage-timings` under `sample_rss.py`, which polls `RssAnon`, `RssFile`, the
process's major-fault count and `read_bytes`, and the box's PSI, every 100 ms.

⊘ **The box was not empty.** Rung 5's GBIF scan ran on the SMB share throughout — network I/O, not
this disk. No other `tessera build` and no serve battery ran during any measured run, which is what
separates these figures from the rung 4 stall they explain.

## The finding

**The rung 4 stall was the access pattern, not the disk contention, and it reproduces at a
fifteenth of the scale.** The text index walked entity space and reached the string arena at a
random offset per document; entity order is signature-then-Morton order and the arena is in the
source's arrival order, so the two are unrelated. At 10⁷ under `MemoryMax=4G` that pass ran for
**over 624 s without finishing** (killed there), against **114 s uncapped** — 441 major faults a
second, **1,922 GiB read for a 13.2 GB arena**, and PSI io `full` at 61–68%: the rung 4 signature.

**Walking the arena instead costs 2% uncapped and 1.33× capped.** The arena record now carries its
entity, the arena remembers a record boundary every 32 MiB or 64 records, and each text worker
reads one contiguous byte range front to back through the descriptor, releasing it behind itself.
The capped stage is **154.58 s** against **116.39 s** uncapped, at **271 major faults** over the
whole stage and **15.7 GiB read** — 1.2× the arena, which is what reading it once looks like. The
bundles are byte-identical.

⊘ **The next stage has the same defect and this change does not fix it.** `record_blob` reads the
same arena by entity and cannot be reordered, because the blob is written in entity order. See §4.

## 1. The reproduction

**It reproduces at 10⁷ under a 4 GB cap, on an otherwise quiet disk, and it reproduces at the same
numbers.**

```
systemd-run --user --scope -p MemoryMax=4G -p MemorySwapMax=0 -- \
  sample_rss.py --out base-cap4g -- tessera build --stage-timings --out bundle-repro
```

Every stage before the text index cost what it costs uncapped — `attribute_tail` **35.95 s**
against 34.79 s, `layers` **108.93 s** against 104.91 s — so the cap on its own is not what the
build is paying for. Then:

| | uncapped | `MemoryMax=4G` |
|---|---|---|
| `text_index` | **68.89 s** | **> 624 s, killed unfinished** — at least **9.1×** |
| major faults | — | **441 a second** |
| `read_bytes` over the stage | — | **1,922 GiB** for a **13.2 GB** arena — **146×** |
| bytes fetched per major fault | — | **7.3 MiB** |
| PSI io `full` | — | **61–68%** |
| `RssFile` | — | **34 MiB – 1.7 GiB**, mostly at the floor |
| `RssAnon` | — | ≤ **1.70 GiB**, a long way inside the cap |

⊘ **The stage was killed at 624 s rather than run to completion**, to leave the box for the other
three arms; it was making progress and would have finished eventually. The claim here is the ratio
and the mechanism, not a wall.

Those are the rung 4 figures: `folio_wait_bit_common`, ~480 major faults a second and PSI io `full`
at 60.8% (`docs/ingest-campaign.md` §4a), reproduced at a fifteenth of the scale with nothing else
on the disk. **The rung 4 stall was not the disk contention.** The contention was real and the
rates there include it; the mechanism is this.

## 2. The attribution

**The walk is not sequential and not local. It is uniformly random.** The baseline binary was
built with a trace over `index_text_chunk`'s entity walk, printing each chunk's arena offsets in
the order it touched them. Over `medcpt-1m-abs`, per 65,536-entity chunk:

| | |
|---|---|
| steps forward vs backward | **32,799 / 32,728** on chunk 0, and within 1% of even on every one of the sixteen |
| mean absolute step | **24.4 MB** over a 96 MB arena — a quarter of the file, which is what a uniform random pair of offsets gives |
| the first sixteen offsets of chunk 0 | 29.2 MB, 92.7 MB, 92.6 MB, 67.0 MB, 44.8 MB, 92.5 MB, 44.9 MB, … |

**The reason is where entity ids come from.** They are assigned in signature-then-Morton order
(`pipeline.rs`, the assignment pass), and the arena is filled in the attribute source's row order.
The two are unrelated, so entity *n* and entity *n+1* are two documents from opposite ends of the
file. This is not a rung 4 property — it is every corpus's, and it is invisible while the arena
fits in memory.

**The kernel reads ahead, which makes it worse rather than better.** Under the cap the walk took
**7.3 MiB of `read_bytes` per major fault**: every fault pulls a large readahead window, the worker
consumes one ~1.2 kB document out of it, and the window is evicted before any other worker wants
it. That is where the 146× amplification comes from — a page-at-a-time walk over a 4 KiB page would
have cost ~3×.

## 3. The fix

**Three changes, and the third is what the first two are for.**

1. **The arena record names its entity.** It was `u32 length ‖ bytes`; it is `u32 entity ‖ u32
   length ‖ bytes`. Four bytes an entity, which at rung 4 is 400 MB on a 119 GB arena, and it is
   what makes the arena readable without the column beside it.
2. **The arena remembers record boundaries** — one every 32 MiB or every 64 records, whichever
   comes first — so it can be split into contiguous byte ranges that each start at a record.
   3,800 offsets at rung 4; the record rule is there so a corpus of short values is divisible at
   all.
3. **A text worker takes an arena range, not an entity range.** It reads it front to back through
   the descriptor into a 4 MiB buffer, declares the range `POSIX_FADV_SEQUENTIAL`, and calls
   `POSIX_FADV_DONTNEED` behind its own cursor a buffer at a time. The mapping's page-table
   entries are dropped once before the pass (`MADV_DONTNEED` over the whole arena), because
   `fadvise` will not evict a page the process still has mapped — without that the walk fills
   memory it cannot give back.

**What it costs is the two places entity ordering used to be free.** Chunks used to partition
entity space ascending, so a run's postings arrived sorted and the merge concatenated them. Now:

* a run's postings are **sorted at the spill**, which is bounded by the worker's byte budget; and
* a term's merged list is produced by **comparing the runs' heads** — a binary heap over at most
  the fan-in — instead of concatenating their records.

The merge's ascent check is unchanged and now does a second job: an entity carried by two runs is
impossible (an entity has one live arena record, and one window holds it) and is refused loudly
rather than encoded as a repeat.

**A record is authoritative only while `at[entity]` still names it.** A value written twice for one
entity leaves the first record in the arena with nothing pointing at it, and indexing that prose
would give the entity terms it does not carry, so the walk checks the offset back against the
column before it yields a record. That check is a random read of eight bytes an entity — 800 MB at
rung 4, three orders of magnitude below the arena — and it is what makes the walk yield exactly
`str_at`'s values and no others.

**Nothing new is persisted and no thread was added.** The window buffers are 4 MiB a worker over
the twelve that already existed.

## 4. What it cost, and what it bought

### The text stage at 10⁷

Two pairs of runs, each pair back to back on the same box state — which matters, and §5 says why.

| `text_index` | uncapped | `MemoryMax=4G` |
|---|---|---|
| before | **114.11 s** | **> 624 s, killed unfinished** |
| after | **116.39 s** (+2.0%) | **154.58 s** |

**The capped stage is 1.33× the uncapped one**, against a bar of 2×, and the mechanism is gone:

| over the text stage, capped | before | after |
|---|---|---|
| major faults | 275,468 (**441/s**) | **271 (1.7/s)** |
| `read_bytes` | **1,922 GiB** — 146× the arena | **15.7 GiB** — 1.2× the arena |
| PSI io `full` | 61–68% | ≤ 16% |
| `RssAnon` | ≤ 1.70 GiB | ≤ 1.74 GiB |

**The peak fell as well as the faults.** Uncapped, the stage used to pull the whole arena into the
resident set: `VmHWM` **14,755 MiB** on the run that had the memory for it, against **8,373 MiB**
after, where the arena is read through a descriptor and never mapped into the walk at all.

### The bytes

`medcpt-10m-abs` built with each binary and compared file by file: **37 of 38 files byte-identical**,
`MANIFEST.json` differing only in `created_at`, and `CURRENT` — which is the manifest's digest —
differing because of it. Same on `medcpt-1m`. `chunking_the_text_index_does_not_change_its_bytes`
still passes over eleven plans, and two cases were added in `column.rs`: the arena windows partition
the records at any window count, over a column written twice at some entities so a stale record is
in the arena; and a value larger than the window's read buffer.

### What did not change, and the stage that now stands out

`attribute_tail` 34.94 → 30.91 s, `layers` 111.05 → 109.84 s, `filter_postings` 21.96 → 24.78 s,
`record_blob` 74.37 → 74.48 s: all within run-to-run variation.

⊘ **`record_blob` has the same defect and is not fixed here.** It walks entities 0..n and reads each
string out of the arena by offset — the identical random access — and it cannot take this fix,
because the blob's rows must be *written* in entity order. It is invisible uncapped (74 s either
way) and it is not invisible under the cap: **> 285 s and still running** at 4 GB, against 74 s
uncapped, at ~120 major faults a second. At rung 4 this is where the build will meet the wall the
text index no longer meets. Fixing it needs the other half of the arena question — an entity-ordered
arena, built at the join with a second pass over the source's text column — which is out of this
change's scope and is the thing to decide next.

## 5. Two things a later reader should not repeat

**The uncapped wall is not a stable number on this box, and the confound is the resident arena.**
The first baseline run measured `text_index` at **77.06 s** with a `VmHWM` of **14,755 MiB** — the
kernel had the memory to hold all 13.2 GB of arena, so the walk's random access cost nothing. The
*same binary*, run an hour later with two 7.2 GB bundles competing for the page cache, took
**114.11 s** at a `VmHWM` of 8,369 MiB. Comparing across that boundary reads as a 49% regression in
a change that has none. Every before/after pair in §4 is two runs back to back.

**Neither `read_bytes / majflt` nor the fault rate alone tells you the walk is random.** The capped
baseline fetched **7.3 MiB per major fault**, which looks like healthy readahead. What it is is
readahead into a random walk: the window arrives, one 1.2 kB document is read out of it, and it is
evicted before anything else wants it. The measurement that settles it is the offset trace in §2 —
forward and backward steps within 1% of even.

## 6. What is here

| | |
|---|---|
| `sample_rss.py` | the resident-split sampler from `probes/2026-09-02-text-peak-split/`, with `majflt`, `read_bytes` and PSI io added |
| `compare_bundles.py` | two bundle trees file by file, excusing `MANIFEST.json`'s `created_at` and the `CURRENT` digest that follows it |
| `*.rss.csv`, `*.stages.csv` | the four measured runs — `base`/`new` × `uncapped`/`cap4g` |
| `base-timed.stages.txt`, `new-timed.stages.txt` | the back-to-back pair of §4's uncapped row, each with a temporary timer splitting the chunk pass from the cascade-and-merge. The timer is not in the shipped code |
