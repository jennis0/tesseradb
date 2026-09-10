# What a build writes to the disk, when it gives it back, and what the forecast could not see

**Date** 2026-09-10. **Branch** `perf/build-disk`, against main at 1de35a20. **Box** WSL2, AMD
Ryzen 9 5900X, 12 cores, 47 GiB, local NVMe-backed VHDX. **Corpus** prefixes of
`data/ladder/gbif`, 16.3×10⁶ to 125.8×10⁶ of its 3,495,729,729 placed GBIF occurrences: one
`geo` view, four declared attributes (a `u8` category, two `keyword`s, a `u16`) and one tiered
taxonomy of three levels.

Rung 6 died at hour three with `bundle/.build-tmp/column-8.arena: No space left on device`, having
taken all 459 GB the box had. Its disk pre-flight had forecast 258,072 MiB — **270.6 GB, half what
the build needed**. This measures what a build of that shape actually writes, removes a third of it,
and makes the forecast a ceiling instead of a half-measure.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    git stash && cargo build --release --bin tessera && cp target/release/tessera /tmp/before
    git stash pop && cargo build --release --bin tessera && cp target/release/tessera /tmp/after
    BEFORE=/tmp/before AFTER=/tmp/after LADDER=data/ladder WORK=/tmp/disk \
      bash probes/2026-09-10-build-disk/run.sh

Every figure is **measured** unless it says otherwise. Disk figures are allocated blocks
(`st_blocks × 512`), sampled over the whole bundle root every 0.5 s; the files these builds write
are reserved with `posix_fallocate` rather than left sparse, so allocated blocks are what the
filesystem has actually given away.

## The result

The peak fell by **a third**, and the forecast went from reading half the truth to reading a third
above it.

| items | peak before | peak after | | forecast before | forecast after |
|---|---|---|---|---|---|
| 16,299,326 | 2.52 GB (154.7 B/item) | **1.96 GB** (120.3) | −22.2% | 1.42 GB (0.56×) | 3.30 GB (1.68×) |
| 30,104,813 | 4.66 GB (154.9) | **3.31 GB** (110.0) | −29.0% | 2.75 GB (0.59×) | 5.00 GB (1.51×) |
| 64,657,133 | 9.54 GB (147.6) | **6.61 GB** (102.2) | −30.7% | 5.36 GB (0.56×) | 9.36 GB (1.42×) |
| 125,789,091 | 19.38 GB (154.1) | **12.70 GB** (100.9) | −34.5% | 9.92 GB (0.51×) | 16.92 GB (1.33×) |

The ratio in brackets is the forecast against the measured peak of the build that binary ran. The
old model read 0.51–0.59 of it at every row count, and the shortfall grew with the corpus; the new
one reads 1.33–1.68, and the margin *falls* with the corpus because what is left of it is
constants.

At 3,495,729,729 items the before column's 154 B/item is **538 GB** (modelled: the fit,
extrapolated, and the whole corpus's characters are 2% longer an item than this prefix's). That is
why rung 6 died on a box with 459 GB.

## What was on the disk at the peak

125,789,091 items, grouped. The before peak is the end of the record blob, the after peak the end
of the filter postings.

| | before | | after | |
|---|---|---|---|---|
| the declared columns and their arenas | 13.19 GB | 104.9 B/item | **8.36 GB** | 66.5 |
| the bundle written so far | 4.17 | 33.1 | **2.83** | 22.5 |
| each view's geometry by ordinal | 1.01 | 8.00 | 1.01 | 8.00 |
| the merged member table | 0.52 | 4.12 | **—** | — |
| the ordinal→entity map | 0.50 | 4.00 | 0.50 | 4.00 |
| | **19.38** | **154.1** | **12.70** | **100.9** |

Two files are most of it. `scientificname`'s arena was **8.59 GB** where the values in it are 4.93,
and `specieskey`'s 2.15 where the values are 1.78 (§"The arena doubled").

## When each file goes, against when it was last read

The whole listing is in `200m-before.files.tsv`; these are the ones whose lifetime is not their
readers'.

| file | B/item | written | last read | unlinked | held for |
|---|---|---|---|---|---|
| `source-ids.u64` | 8.00 | pass one | the layer publication | the layer publication | — |
| `x-anchor.u32`, `y-anchor.u32` | 8.00 | the geometry pass | the signature sort | the assignment | — |
| `x-of-ordinal-0.u32`, `y-of-ordinal-0.u32` | 8.00 | the geometry pass | the view's permutation | **the end of the build** | 42 s of 327 |
| `member-table.spill` | 4.12 | the layer publication | the layer publication | **the end of the build** | 113 s of 327 |
| `column-2.at`, `column-3.arena` (`specieskey`) | 20.8 | the attribute join | the filter postings | **the record blob** | 34 s of 327 |
| `keyword-ordinals.scratch` | 3.77 | the keyword dictionary | the values file | the values file | — |
| `column-0.col` (`kingdom`, `render`) | 1.13 | the attribute join | the segment write | the end of the build | 46 s of 327 |

`column-8.arena`, the file rung 6 died writing, is `scientificname`'s — the **fourth declared
column's** arena and not an internal one. `ColumnScratch` serials count files, not columns: four
declared attributes take ten of them (a fixed-width column is `col` and `present`, a string one is
`at`, `arena` and `present`), and the attribute join's staging buffer takes ten more.

**Nothing is written that is never read.** Every file the sampler saw has a reader in the build
that wrote it, `pairs.parquet` included (the test-time oracle's, which `--no-oracle-pairs` skips).

**One thing was written twice in two forms.** `x-anchor.u32` and `y-anchor.u32` are the anchor
view's Morton geometry with the declared fallback (decision 0112) applied. On a corpus whose anchor
holds every item — every single-view corpus — the fallback reaches nothing and the two files are a
value-for-value copy of `x-of-ordinal-<anchor>.u32` and `y-of-ordinal-<anchor>.u32`.

## What the forecast was missing

Five errors, all in the same direction. Sizes at 3,495,729,729 items are modelled by scaling the
measured per-item figure; the two payload figures are the pre-flight's own sample of the whole
corpus.

**1. A Parquet column's uncompressed size is not its characters, and for these columns it is not
close.** The model read the footer, which for a dictionary-encoded string column reports the
encoded page size: its indices and its dictionary, not the values one `String` an entity expands
to.

| column | footer says | the values are | | at rung 6 |
|---|---|---|---|---|
| `scientificname` (`keyword`) | 7.22 B/item | **31.22** | 4.3× | 21.6 GB charged against 111.3 |
| `specieskey` (`keyword`) | 2.14 | **6.60** | 3.1× | 6.1 GB charged against 22.6 |

That one term is **+105 GB** at rung 6, and it is the term the build died inside.

**2. A tiered layer's member row is not a member pair.** The model counted the member file's
Parquet rows. GBIF's file is 3,495,729,729 rows whose `key` column is a list of three, carrying
**10,014,654,968** `(artifact, source)` pairs. The spill and the packed extents are per pair. The
footer says so directly: the list leaf's `num_values` is 377,367,273 over the 125,789,091-row
prefix, exactly 3×. At rung 6 the two member terms were charged 33 GB against a modelled 105.

**3. The bundle was not modelled.** The pre-flight priced the build's scratch and, of the bundle,
one 4 B/pair term for the postings. The bundle only grows: **22.5 B/item by the column phase and
68.5 when it is finished**, measured at four row counts and agreeing to within 1%. At rung 6 that
is 79 GB standing at the peak that the forecast did not name, and a 244 GB bundle at the end. The
run that died had 93 GB of it written.

**4. The arena doubled.** `MappedArena` grew by doubling and reserved its blocks, so an arena whose
values stop just past a power of two costs twice what it carries. Measured: `scientificname`'s is
8.59 GB allocated over 4.93 GB written — **3.66 GB of blocks nothing ever wrote**, 19% of that
build's whole peak. Nothing modelled it, and a model that had would have had to charge twice the
payload to stay a ceiling.

**5. The attribute join's staging buffer is a second set of columns.** `JOIN_STAGE_BYTES` bounds it
at 256 MiB of fixed-width slots, which for this schema is 3,579,139 rows a chunk — but the budget
prices a string slot at a `String` header and the chunk's characters go into arenas of their own.
Measured at 405 MB, flat in the corpus. Small, and it was the whole difference between the
16.3×10⁶ build's peak and the moment after it.

## What was changed

Six changes in `tessera-build`. Each is scratch or lifetime; none touches a published byte.

### 1. The arena grows by a step, not by doubling

`MappedArena::grow` doubles while the arena is under `ARENA_GROWTH_STEP` (256 MiB) and adds a step
at a time above it, and `reserve` now `posix_fallocate`s the new range rather than the whole file.
The capacity is then within one step of the payload, which is both smaller and — the reason it
matters more — **modellable**: `residency` charges an arena its characters and a constant where it
would otherwise have to charge them twice.

At 125,789,091 items: `scientificname`'s arena 8.59 → 4.56 GB, `specieskey`'s 2.15 → 1.34. The two
changes below share the credit: the narrower record header takes 0.50 GB out of what is written,
and the step is what stops that 0.50 GB being rounded back up.

### 2. A `keyword` or `utf8` record carries no entity

The arena's record header was a `u32` entity and a `u32` length. The entity is what makes the arena
readable in its own order, and the only column read that way is a group-scoped `text` one, which
has no blob row to be read from instead (`views.md` §5). Every other string column is reached at an
entity and nowhere else. The declared type now decides the header — 8 bytes for `text`, 4 for
`keyword` and `utf8` — and `EntityColumn::for_each_record_in` refuses a column that carries no
entity rather than decoding one out of a length.

4 B/item on each such column: 0.98 GB at 125,789,091 items (the two columns' present rows), and
28.0 GB at rung 6 (modelled, items × 4 B on each of the two).

### 3. A column goes back at its last reader, not at the release stage

`column_release` ran after the record blob and released every non-render column at once. A column
with no blob row — an `index = true` keyword, say — met its last reader when the filter postings
ended, two stages earlier. It is released there now, and the release stage keeps the rest.
`write_record_blob` takes the item count as an argument rather than reading it off
`by_entity.first()`, which a released first column would have answered zero for.

At 125,789,091 items that is `specieskey`'s 2.35 GB and `year`'s 0.27 off the record blob's window:
the measured blob-phase peak is 11.39 GB against the index phase's 12.70. At rung 6 the same column
is 62,240 MiB (modelled, the pre-flight's own figure).

### 4. The anchor's geometry is the anchor view's, where the anchor holds every item

A view's ids are distinct — pass one checks each view's segment as it fills it — so a view whose row
count is the union's holds every ordinal, the fallback reaches nothing, and the arrays that would be
written are the ones already on the disk. `build` reads those instead. 8 B/item of reserved disk and
an `n`-length copy, on every single-view corpus.

### 5. Each view's ordinal geometry goes at its permutation

`geometry` was dropped after the view loop. A view's `x`/`y` arrays have served their only reader —
the scatter into entity space — as soon as that view's permutation is built, and the presence bits
and row count read after it are a bit an item rather than eight bytes. 1.01 GB at 125,789,091 items,
from the tiler sort to the end of the build: measured 42 s of a 327 s build, and at rung 6 the
artifact pass and the manifest digest are most of an hour.

### 6. The member table unlinks itself

`spill::MemberTable` had no `Drop`, so `member-table.spill` stood from the layer publication to
`TmpDir::close`. Its only reader is the publication; the artifact pass four stages later reads the
packed extents the store wrote. 518 MB over 125,789,091 items, held for a third of the build.

## What the model says now

Six phases rather than four, because the column window was three windows: the attribute join and the
publication, the filter postings, and the record blob hold different files. Every term names the
phases it stands through and the forecast is the largest phase.

125,789,091 items, the `after` binary. Modelled against measured, per phase:

| phase | measured | modelled | |
|---|---|---|---|
| spill | 2.01 GB | 2.77 | 1.37× |
| band | 3.78 | 5.28 | 1.40× |
| join | 12.39 | **16.92** | 1.37× |
| index | **12.70** | 15.35 | 1.21× |
| blob | 11.39 | 14.06 | 1.23× |
| assembly | 9.11 | 13.93 | 1.53× |

The model puts the peak in the join phase and the measurement puts it in the index phase, 2.5%
apart — the two are a tie at this schema, and the phase the refusal names is the one whose terms it
prints.

**It is a ceiling now and it was a lower bound before.** Three terms carry most of the margin and
each is a stated ceiling rather than a measurement: `postings.arrow` and `pairs.parquet` at 4 B/pair
(this corpus's 253 country terms encode to 34 KB against a charged 503 MB), the record blob's
blocks at half its columns' characters (measured at 0.26), and the member extents at 3 B an entry
(measured 2.24).
An operator refused a build can read which of them they are being refused for.

⊘ **Rung 6 itself is modelled and not built**, and both binaries were run over the whole corpus
to the pre-flight and stopped there. MiB:

| | before | after |
|---|---|---|
| **the forecast at peak** | **258,072** (the column phase) | **410,639** (the join phase) |
| `scientificname`'s column | 74,318 | 146,801 |
| `specieskey`'s column | 59,573 | 62,240 |
| the member spill's runs and table | 26,670 | 40,005 |
| the published member extents | 6,667 | 30,004 |
| the sorted source ids | 26,670 | 26,670 |
| the ordinal→entity map | 13,335 | 13,335 |
| the bundle's own files | not modelled | modelled, term by term |

The six phases, after: spill 84,011, band 140,019, **join 410,639**, index 388,180, blob 361,135,
assembly 367,687 MiB. **410,639 MiB is 430.6 GB against the 459 GB the box had when rung 6 started**,
so the pre-flight admits the build with about 28 GB of headroom where the peak it forecasts is a
ceiling: the fit from the measured builds puts the real peak near 360 GB (modelled, 101 B/item
against 3,495,729,729 and 2% for the whole corpus's longer values).

The memory figure is unchanged, at 13,399 MiB. The member entry count is the disk terms'
denominator alone; the publication's own Roaring is charged over the member file's **rows**, which
is a ceiling on any one level's entries and is what it was charged over before.

⊘ The entry count the footer gives is 10,487,189,187, three per row. The corpus actually carries
10,014,654,968 (`data/ladder/gbif/manifest.json`), a row whose key list is short being counted at
the level count where the writer stores it as a null. The model reads 4.7% high on every
per-entry term.

## The bundle is the same bundle

Byte-identical on eight corpora, each built with both binaries and compared file by file
(`docs/ingest-campaign.md` §4c). In every case the only files that differ are `MANIFEST.json`, in
`created_at` alone and checked field by field, and the `CURRENT` that carries its digest.

| corpus | items | files | what it covers |
|---|---|---|---|
| `gbif-25m` … `gbif-200m` | 16.3–125.8×10⁶ | 32 | one view, a tiered layer of three levels, two `keyword` columns, one indexed |
| `multiview` | 21,300 | 111 | **ten views**, ids sparse over a 13.5×10⁶ span, and a **group-scoped `text` column** — the one family still read in arena order, and the only reader of the wider record header |
| `treeoflife-1m` | 1,000,000 | 61 | two views, the second sparse, a tiered taxonomy |
| `medcpt-1m` | 1,000,000 | 36 | a `text` column with a token index, prose as record-blob extents |
| `geonames` | 13,463,857 | 70 | two tiered layers over two member files, eight vocabularies, thirteen declared columns |

`cargo test --workspace --no-fail-fast`: 2,825 passed, 0 failed, 23 ignored.

## What is not measured

⊘ **Rung 6 was not built.** Nothing here was measured above 1.26×10⁸ items. Every rung-6 figure is
the fit or the model extrapolated, and the whole corpus's `scientificname` averages 31.84 B/item
against this prefix's 31.22, so a fit from the prefix reads about 2% low on that term.

⊘ **The wall clock moved by less than the box did.** Whole-build elapsed, before against after:
−2.9%, +2.5%, +1.8%, +4.7% at the four row counts. A second pair at 64,657,133 items, run later on
the same box, was 168.7 s and 167.7 s against the first pair's 157.0 and 159.8 — a **7.5% spread on
the unchanged binary alone**, wider than any of the four differences. No claim is made for the
difference in either direction. The changes remove writes and add `mmap` pairs (a 137 GB arena takes
about 512 growths at rung 6 where doubling took 17), and neither effect was isolated.

The **peak** does reproduce: that second pair read 9.541 GB and 6.611 GB, the same figures to the
byte as the first.

⊘ **Nothing was measured under disk pressure.** Every run had over 380 GB free, so no allocation
ever failed and the `posix_fallocate` refusal these changes make smaller was never reached.

⊘ **The forecast's margin is not uniform across schemas.** Its three largest ceilings —
`4 B/pair` for the postings, half the characters for the record blob, 3 B for a member entry — were
each checked against this corpus and one or two others, and a corpus whose access relation is
scattered over many terms would spend the first of them where GBIF's 253 country codes do not.

⊘ **`keyword-run-*.spill` and `keyword-ordinals.scratch` live in the bundle tree, not
`.build-tmp/`.** They are unlinked by the pass that writes them, so a build killed inside the
keyword dictionary leaves them behind where `TmpDir::create` would have swept them. Measured at
5.3 B/item together, and charged at 8; left where they are.

⊘ **The remaining lever is the largest one.** `scientificname` is a `keyword` column with neither
`index` nor `render`, so its only reader is the record blob — and it costs 5.7 GB of entity-ordered
arena and offsets at 125,789,091 items to hand the blob values it then writes as 1.55 GB of
compressed extents. A `text` column with the same readers takes the prose route instead
(`build-prose-extents.md`): spilled as extents while the join decodes it, never placed at an entity
index. Routing a blob-resident `keyword` column the same way would remove the largest single file
in the build. It changes which pass produces the blob's input, so it is an owner's call rather than
a performance one.
