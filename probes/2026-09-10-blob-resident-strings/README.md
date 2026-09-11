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
([`build-column-extents.md`](../../docs/design/build-column-extents.md) §2 as it stood). This
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
(`build-column-extents.md` §8): what the route buys at a rung whose arena fits in memory is the peak
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

## 2026-09-11: which route, and what the choice is keyed on

**Date** 2026-09-11. **Branch** `perf/extent-route-threshold`, against main at 1db5d16e. Same box,
same corpora, same slicer and sampler. The question is the one the section above left open: the
extent route saves a quarter of the peak disk and costs 13% of the entity-order stages at a rung
whose arena fits, so which rungs should take it.

Two binaries, both from this branch, differing in one token — `build` calling `pipeline::build`
with `ExtentRoute::Arena` or `ExtentRoute::Extents` rather than `Derived`. Everything else is held
still: the same memory budget, the same batch stride, the same band and dictionary plans. Varying
the free space or the budget instead would have varied the plan as well.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    # three binaries: the derived route, and one forced down each
    cargo build --release --bin tessera            # tessera-derived
    sed -i 's/ExtentRoute::Derived/ExtentRoute::Arena/' crates/tessera-build/src/lib.rs
    cargo build --release --bin tessera            # tessera-arena, then restore and repeat for Extents
    ARENA=/tmp/tessera-arena EXTENTS=/tmp/tessera-extents DERIVED=/tmp/tessera-derived \
      LADDER=data/ladder WORK=/tmp/routes bash probes/2026-09-10-blob-resident-strings/routes.sh

## The arena route is faster everywhere it was measured

Quiet box — no foreign process over 20% of a core before or after any run, checked either side of
each — oracle pairs written, the derived memory budget (36.5 GiB ± 0.4). Wall clock is the whole
build; peak disk is allocated blocks over the bundle root, sampled every 0.5 s.

| items | arena wall | extents wall | | arena peak | extents peak | |
|---|---|---|---|---|---|---|
| 16,299,326 | **38.6 s** | 42.0 | +8.8% | 1.94 GB | **1.38** | −29% |
| 30,104,813 | **73.7** | 78.5 | +6.5% | 3.31 | **2.37** | −28% |
| 64,657,133 | **162.3** | 170.6 | +5.1% | 6.61 | **4.59** | −31% |
| 125,789,091 | **324.7** | 356.1 | +9.7% | 12.60 | **9.26** | −26% |

The route the derived rule takes at all four is the arena. Both binaries' peaks agree with the
section above's to within 3%, which is that section's own sampler margin.

Per stage at 125,789,091 items: the arena route's `attribute_tail` is 42.6 s against 58.0 and its
`record_blob` 28.9 against 44.5, where `filter_postings` is 25.2 against 24.4. The extent route
pays zstd twice — once in the join's own lane and once decompressing and recompressing at the
merge — and the arena route pays neither.

## There is no crossover in memory, because the build OOMs first

`--memory-budget 4g` on every run, so the plan is one build and only the page cache moves;
`--no-oracle-pairs`; a `systemd-run --scope` with `MemoryMax` and `MemorySwapMax=0`. Wall seconds.

| cap | 30.1×10⁶ arena | extents | 125.8×10⁶ arena | extents |
|---|---|---|---|---|
| none (46 GiB available) | **73.7** | 78.5 | **324.7** | 356.1 |
| 8 GiB | **72.4** | 78.3 | **352.1** | 364.9 |
| 6 GiB | **72.2** | 79.6 | **379.9** | 383.9 |
| 5 GiB | — | — | OOM-killed | — |
| 4 GiB | 79.8 | **79.1** | OOM-killed | — |
| 3 GiB | **85.0** | 93.2 | — | — |

The 30.1×10⁶ pair at 4 GiB is the one point of the sweep where the two routes are level, and the
next cap down parts them again in the arena's favour. The uncapped 30.1×10⁶ and 125.8×10⁶ rows are
the derived budget rather than the 4 GiB one, so they are the trend's end and not a fifth point on
its curve.

**The arena's reads do not degrade under pressure.** At 125,789,091 items `record_blob` is 28.9 s
uncapped, 28.8 s at 8 GiB and 28.7 s at 6 GiB on the arena route, against 44.5, 39.3 and 40.0 on
the extent route — three caps spanning a factor of eight in page cache, and no trend. An arena is a permutation of the source, but the join writes each chunk of it in
that chunk's own entity order, so the blob's ascending merge reads it as a few dozen ascending runs
rather than at random — which is what `probes/2026-09-03-entity-ordered-arena/` found for the fill
order and is why the page cache holding less of it costs nothing. What does degrade under a cap is
the join: `attribute_tail` 42.6 s uncapped, 66.3 at 8 GiB, 77.5 at 6 GiB.

⊘ **Below 6 GiB the 125.8×10⁶ build cannot run at all.** At 5 GiB it is OOM-killed in `tiler_sort`
at 5,072 MiB and at 4 GiB in `filter_postings` at 4,064 MiB — stages whose memory no route reaches.
The modelled entity-order window is 13,063 MiB there, so the cap the build dies at is 2.1× under
the window, and the route cannot be the thing that decides whether a build fits memory.

## So the route is keyed on the disk

The choice is per column, at the plan: a column with two routes moves onto the arena and stays
there while the entity-order stages' largest phase still models inside half the space free on the
output filesystem. Half, because the filesystem is shared and the choice stands for the whole
build.

The modelled scratch on this schema, arena route, falls with the corpus as the per-column constants
amortise: 131.3 B/item at 16.3×10⁶, 116.6 at 30.1×10⁶, 108.7 at 64.7×10⁶ and 104.3 at 125.8×10⁶.
Extrapolating on the last of those, the crossover is at

| free space | crossover |
|---|---|
| 190 GB | 0.91×10⁹ items |
| 380 GB | 1.82×10⁹ |
| 459 GB, an empty disk | 2.20×10⁹ |

and 3,495,729,729 rows model at 365 GB of scratch, which spills at any free space this box has had.
All four slices took the arena, at 2,041 / 3,346 / 6,702 / 12,519 MiB of modelled scratch against
190,515 to 228,956 MiB free.

⊘ **Rung 6 is modelled and not built**, from the per-item fit above.
`residency::tests::the_gbif_rung_spills_where_the_slice_that_fits_does_not` holds the two ends of
that table against the rule.

⊘ **Free space is not a constant, and the crossover moves with it.** The output filesystem held
between 190 and 381 GB free over the course of these runs, which moves the crossover by a factor of
two. Every corpus in the ladder is two orders of magnitude below it and every route above is the
same either way, but a corpus near that line would route two ways on two days — byte-identical, and
5 to 10% apart on the clock. The build prints the route, the modelled scratch and the free space it
was decided against, which is what makes two such runs readable against each other.

## The bundle is the same bundle, down either route

Each corpus built with both binaries and compared file by file. In every case the only files that
differ are `MANIFEST.json`, in `created_at` alone and checked field by field, and the `CURRENT`
that carries its digest.

| corpus | items | files | the route actually differed |
|---|---|---|---|
| `gbif-64p` | 25,846,007 | 32 | **yes** — `scientificname` took the arena down one and extents down the other |
| `multiview` | 21,300 | 111 | no bundle-wide string column has two routes; its `text` column is group-scoped |
| `treeoflife-1m` | 1,000,000 | 61 | no — `common_name` is `text` and spills either way |
| `medcpt-1m` | 1,000,000 | 36 | no — `title` and `mesh_major` are `text` |
| `geonames` | 13,463,857 | 70 | no — `name` is `text` |

Only `gbif-64p` carries a column the choice reaches, so the other four are a regression check on
the routing rather than a check of it. `crates/tessera-build/tests/extent_route.rs` is the case no
ladder corpus has: one build carrying a blob-resident `keyword`, an indexed `keyword` and a `text`
column, forced down both routes, byte-identical, with the routes read back from the build's report
rather than assumed.

## What is not measured

⊘ **Nothing was measured above 1.26×10⁸ items**, and nothing above a 5.71 GB arena. The claim that
the arena's reads stay sequential is structural — it is the join's chunk sort — but the largest
arena it has been checked on is that one.

⊘ **The cap sweep squeezes the page cache and not the disk.** No run was made with the output
filesystem near full, which is the condition the route exists to avoid.

⊘ **One column, one value distribution**, as above: `scientificname` is 757,711 distinct values over
125,789,091 rows. A blob-resident column of near-unique values spills more and saves less, and
would reach the crossover at a smaller corpus.
