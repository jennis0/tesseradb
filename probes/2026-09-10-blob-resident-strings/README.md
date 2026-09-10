# A string column the record blob alone reads costs 44 bytes an item to deliver 13

**Date** 2026-09-10. **Branch** `perf/blob-resident-strings`, against main at aa258c15. **Box**
WSL2, AMD Ryzen 9 5900X, 12 cores, 47 GiB, local NVMe-backed VHDX. **Corpus** prefixes of
`data/ladder/gbif`, 16.3×10⁶ to 125.8×10⁶ of its 3,495,729,729 placed GBIF occurrences: one `geo`
view, four declared attributes (a `u8` category, two `keyword`s, a `u16`) and one tiered taxonomy
of three levels.

`scientificname` is one of those keywords, declared with neither `index` nor `render`, so the
record blob is the only thing that reads it. The build gave it an entity-ordered arena and an
offset array anyway, because the route was decided by the declared type: `text` took the record
blob's extents and every other string family took an arena
([`build-prose-extents.md`](../../docs/design/build-prose-extents.md) §2 as it stood). This
measures what that cost, routes the column by its readers instead, and checks the bundle is the
same bundle.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    git stash && cargo build --release --bin tessera && cp target/release/tessera /tmp/tessera-before
    git stash pop && cargo build --release --bin tessera && cp target/release/tessera /tmp/tessera-after
    BEFORE=/tmp/tessera-before AFTER=/tmp/tessera-after LADDER=data/ladder WORK=/tmp/blob \
      bash probes/2026-09-10-blob-resident-strings/run.sh

The slicer and the disk sampler are [`probes/2026-09-10-build-disk/`](../2026-09-10-build-disk/README.md)'s,
called rather than copied. Every figure is **measured** unless it says otherwise. Disk figures are
allocated blocks (`st_blocks × 512`), sampled over the whole bundle root every 0.5 s.

## The result

The peak fell by about a quarter at every row count, and at 125.8×10⁶ it stopped being the
build's scratch at all.

| items | peak before | peak after | |
|---|---|---|---|
| 16,299,326 | 1.93 GB (118.3 B/item) | **1.39 GB** (85.2) | −28.0% |
| 30,104,813 | 3.20 GB (106.4) | **2.38 GB** (78.9) | −25.8% |
| 64,657,133 | 6.56 GB (101.5) | **4.64 GB** (71.7) | −29.4% |
| 125,789,091 | 12.48 GB (99.2) | **9.26 GB** (73.6) | −25.8% |

**The one column is the whole of it.** At 125,789,091 items `scientificname` carries 3,927,078,659
characters, 31.22 B/item (measured over the slice's own Parquet).

| | before | | after | |
|---|---|---|---|---|
| the offset array (`column-7.at`) | 1.01 GB | 8.00 B/item | — | — |
| the arena (`column-8.arena`) | 4.56 | 36.28 | — | — |
| the extents' blocks | — | — | **1.07** | 8.53 |
| the extents' row directories | — | — | **0.52** | 4.13 |
| the extents' has-row bitmaps | — | — | **0.04** | 0.30 |
| | **5.57** | **44.28** | **1.63** | **12.97** |

36 extents, one per join chunk. The blocks are 0.273× the characters, the whole extent family
0.415×, where the arena and its offsets are 1.418×.

Extrapolated to rung 6's 3,495,729,729 rows the column is **155 GB against 45** (modelled: the
per-item figures above, and the whole corpus's values are 2% longer an item than this prefix's).

## Where the peak is now

125,789,091 items, the peak listing grouped. The before peak is at t=244 s, at the end of the
filter postings; the after peak is at t=370 s, inside the manifest digest.

| | before | | after | |
|---|---|---|---|---|
| the declared columns, their arenas and their extents | 8.36 GB | 66.5 B/item | **0.14** | 1.1 |
| the bundle written so far | 2.61 | 20.8 | **8.62** | 68.5 |
| the rest of `.build-tmp/` | 1.51 | 12.0 | **0.50** | 4.0 |
| | **12.48** | **99.2** | **9.26** | **73.6** |

**What is left is the bundle, and nothing releases it.** 68.5 B/item is the finished bundle on this
schema, the same figure `probes/2026-09-10-build-disk/` measured at four row counts. The build's
own scratch is no longer what a corpus of this shape is refused for.

## What the forecast says

`residency.rs` charges a spilled column its presence bits and half its characters, where it
charged an arena the characters, the record headers and 8 B an item of offsets. Which columns take
which term is `pipeline::takes_extents`, called by the model rather than restated in it.

| items | forecast before | forecast after | measured after | |
|---|---|---|---|---|
| 16,299,326 | 3,149 MiB | **2,461** | 1,324 MiB | 1.86× |
| 30,104,813 | 4,772 | **3,714** | 2,265 | 1.64× |
| 64,657,133 | 8,924 | **6,907** | 4,421 | 1.56× |
| 125,789,091 | 16,139 | **13,287** | 8,831 | 1.50× |

The forecast stays a **ceiling**, which is what it is for: it is compared against free space and
an under-read is an ENOSPC at hour three. It reads 1.50–1.86× the measured peak; over the arena
route the same model read 1.36–1.71×. The margin widens because the term the route removes was the
best-modelled one — an arena's capacity is known to within one growth step, where what is left of
the margin is the three stated ceilings `probes/2026-09-10-build-disk/` names: the postings at 4 B
a pair, the record blob at half its columns' characters, and a member entry at 3 B. Those three
now carry the whole of it, which is why the ratio no longer falls with the corpus.

⊘ **Rung 6 is modelled and not built.** Both binaries were run over the whole corpus to the
pre-flight and stopped there. MiB:

| phase | before | after |
|---|---|---|
| spill | 84,011 | 84,011 |
| band | 140,019 | 140,019 |
| join | **410,639** | 317,316 |
| index | 388,180 | 294,856 |
| blob | 361,135 | 267,812 |
| assembly | 367,687 | **367,687** |

**430.6 GB becomes 385.5 GB**, and the phase the refusal would name moves from the attribute join
to the assembly. The join's own figure falls by 93,323 MiB, and `scientificname`'s column is the
whole of that: `probes/2026-09-10-build-disk/` prints the same binary's term for it at 146,801 MiB,
which leaves 53,478 for the extents. What now decides whether rung 6 starts is the finished bundle
and the row spaces, not the columns.

## The bundle is the same bundle

Byte-identical on five corpora, each built with both binaries and compared file by file
(`docs/ingest-campaign.md` §4c). In every case the only files that differ are `MANIFEST.json`, in
`created_at` alone and checked field by field, and the `CURRENT` that carries its digest.

| corpus | items | files | what it covers |
|---|---|---|---|
| `gbif-64p` | 25,846,007 | 32 | **the blob-resident `keyword`**, beside an indexed one, and a tiered layer of three levels |
| `multiview` | 21,300 | 111 | ten views, ids sparse over a 13.5×10⁶ span, and a **group-scoped `text` column** — the family that keeps its arena |
| `treeoflife-1m` | 1,000,000 | 61 | two views, a tiered taxonomy, two indexed `keyword`s and a `text` column |
| `medcpt-1m` | 1,000,000 | 36 | two `text` columns with token indexes, and an indexed `keyword` |
| `geonames` | 13,463,857 | 70 | two tiered layers over two member files, eight vocabularies, thirteen declared columns, one indexed `text` |

The build's own report agrees too: on `gbif-64p` the two logs match line for line apart from the
`disk:` forecast, the stage times and the output path — the same coverage counts, the same artifact
layouts, the same index sizes.

## What it costs

⊘ **It is slower at every row count measured here, and that is expected.** The whole prefix fits
the page cache on a 47 GiB box, so there is no I/O to save, and the join pays zstd on the column's
own lane while the record blob decompresses the extents and recompresses them. 125,789,091 items,
wall seconds:

| stage | before | after |
|---|---|---|
| `attribute_tail` | 45.1 | 61.6 |
| `filter_postings` | 26.8 | 26.7 |
| `record_blob` | 28.9 | 49.0 |
| the sum of the stages | 296.5 | **335.2** |
| the largest resident set a stage reported | 11,212 MiB | **9,817 MiB** |

That is the same shape the prose route measured at 10⁷ and for the same reason
(`build-prose-extents.md` §8): what the route buys at a rung whose arena fits in memory is the peak
— 12% of the resident set here and a quarter of the disk — and what it buys at a rung whose arena
does not is the build finishing. Rung 6's `scientificname` arena is 155 GB (modelled) on a box with
47 GiB of memory and 459 GB of disk.

## What is not measured

⊘ **Rung 6 was not built.** Nothing here was measured above 1.26×10⁸ items. Every rung-6 figure is
the model or the per-item fit extrapolated.

⊘ **The sampler under-reads a transient, and by a few percent.** The peak is a maximum over
0.5-second samples, so a capacity that stands for less than that can be missed. The same
16,299,326-item build read 1.93 GB in 79 samples on a quiet box and 2.12 GB in 93 samples while two
other builds ran — the slower run caught more of the curve, not a different curve.
`probes/2026-09-10-build-disk/` reports 1.96 GB for the binary this one calls `before`, within 2%
of the quiet figure here. The before/after pairs in this probe were each run back to back on an
otherwise idle box.

⊘ **One corpus, one column, one value distribution.** `scientificname` is a repetitive keyword —
757,711 distinct values over 125,789,091 rows, 166 rows a value — so its blocks compress to 0.273×
where prose measured 0.34× (`probes/2026-09-04-rung-4-whole/`). A blob-resident column of
near-unique values, a DOI or a URL, would spill more and save less. The model charges half the
characters for both and is a ceiling over each.

⊘ **Nothing was measured under disk pressure.** Every run had over 330 GB free, so no allocation
ever failed.

⊘ **A `utf8` column was not built.** It is not declarable from TOML, so the route's `utf8` arm is
covered by `pipeline::tests::the_extent_route_is_the_readers_and_not_the_type` and by no build.
