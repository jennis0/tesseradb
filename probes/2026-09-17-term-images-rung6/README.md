# Term images at rung 6

Status: complete, 2026-09-18. Parts A, B and C. The corpus is prepared, both rungs are built at
bundle format 14, the four routes are measured over the whole 3,495,729,729-row corpus under a
24 GiB cap, and the chooser's five constants are re-derived from those figures. The sections below
are in the order they were run, so the Part A pre-flight and the refused build of 2026-09-17 stand
where they were written and the measurements that answer them follow. Not normative; re-take a
figure before relying on it.

Box: WSL2, 12 cores, 47 GiB in the VM, local NVMe. Commit `032b8296` for the two builds below
(`tessera build --memory-budget 24g --no-oracle-pairs --stage-timings`, `nice -n 10`, under
`setsid`). Neither 64-part build is capped: each holds 2.8 GiB and the cap exists for the rung 6
bundle, which nothing here opened.

## What was asked

Ruling D rebuilds rung 6 with each row's access list `[country, "y:"+year, "s:"+specieskey]`, so
that the route measurement has principals shaped like a user's term set over a real dictionary.
Before committing to a build of that size, the ruling asks for the postings size, the build time
and the free disk. This directory holds the corpus change, the 64-part build the model is corrected
by, and the pre-flight.

## The corpus

`test_corpora/gbif/prepare.py --from-points <rung>` rewrites a prepared rung's `points.parquet`
with the access column and writes the rest of the rung beside it. The access terms come from the
`countrycode`, `year` and `specieskey` the file already carries, so the 258 GB source share is not
read again. Entity ids keep the values the first pass gave them and the run checks them contiguous
per row group; `.env` is copied, so the rewritten rung has the source rung's identity key and the
row order it fixes. `members-taxonomy.parquet` and `vocab-kingdom.parquet` are hard links: a
prepared rung's files are written once and never edited, and a copy of the 22.9 GB member file
would cost the disk the build needs.

Measured, from the two rewrites:

| | `gbif-64p-terms` | `gbif-terms` |
|---|---|---|
| rows | 25,846,007 | 3,495,729,729 |
| access terms | 186,178 | 1,399,205 |
| country / year / species terms | 252 / 508 / 185,418 | 253 / 527 / 1,398,425 |
| access pairs | 75,023,354 | 10,124,084,726 |
| pairs a row | 2.903 | 2.896 |
| `points.parquet` | 430.2 MB → 519.5 MB | 55.05 GB → 66.52 GB |
| rewrite wall | 13.4 s | 32.9 min |

The rung 6 term and pair counts are exact rather than extrapolated: the terms are the country,
year and species-key distinct counts the first pass's census recorded, and the pairs are the rows
with a value in each of the three columns.

## The 64-part build, country only against terms

Two builds of the same 25,846,007 rows on the same commit, one with `point_visibility` on
`countrycode` (`gbif-64p-base`, a scratch rung whose data files are hard links to `gbif-64p`) and
one on the access list (`gbif-64p-terms`). Everything but the access column is identical, so the
column ratio is what the access change costs at a fixed row count. All figures measured, seconds.

| stage | country only | terms | ratio |
|---|---|---|---|
| `source_ids` | 0.73 | 1.34 | 1.84 |
| `dictionary` | 1.62 | 18.30 | 11.30 |
| `geometry_read` | 3.69 | 9.83 | 2.66 |
| `signature_sort` | 0.86 | 1.89 | 2.20 |
| `assignment` (two passes) | 6.49 | 9.03 | 1.39 |
| `postings_write` | 0.38 | 1.54 | 4.05 |
| `attribute_tail` | 12.45 | 11.04 | 0.89 |
| `layers` | 14.31 | 14.61 | 1.02 |
| `filter_postings` | 6.33 | 5.60 | 0.88 |
| `record_blob` | 3.34 | 2.85 | 0.85 |
| `column_release` | 0.12 | 0.16 | 1.33 |
| `tiler_sort` | 2.23 | 2.47 | 1.11 |
| `segment_write` | 5.47 | 5.15 | 0.94 |
| `artifact_pass` (two passes) | 10.64 | 8.62 | 0.81 |
| `term_images` | 0.13 | 6.63 | 51.0 |
| `manifests` | 0.24 | 0.22 | 0.92 |
| **sum of stages** | **69.03** | **99.28** | **1.44** |

Stage peak `VmHWM` 2,774 MiB in both; the figure includes mapped file pages, so it is not the
anonymous peak.

The term images, from the build report line (ruling G), one view `geo`:

| | country only | terms |
|---|---|---|
| terms | 253 | 186,179 |
| kept | 246 (97.2%) | 5,516 (3.0%) |
| payload | 92.21 kB | 76.61 MB |
| table | 10.12 kB | 7.45 MB |
| largest image | 7.25 kB | 2.66 MB |
| derivation wall | 0.1 s | 6.6 s |
| `.timg` on disk | 102,485 B | 84,052,461 B |

The bundle grew from 1,456,908,819 B to 1,662,295,444 B, +14.1%. Where the bytes went, measured:

| file | change |
|---|---|
| `entities/terms/terms.u32` | +196.7 MB |
| `term-images/…​.timg` | +83.9 MB |
| `terms/postings.arrow` | +20.4 MB |
| `dictionary/terms-0.dict` | +2.4 MB |
| `attrs/record/blocks.bin` | −92.1 MB |
| `attrs/{year,specieskey}/presence.roaring` | −2.3 MB |
| members | −3.7 MB |

`terms.u32` is one `u32` per pair, so it follows the pair count exactly. The record blob and the
two presence bitmaps shrank because entity order is signature order: with three terms in a
signature, rows sharing a country, a year and a species key are adjacent, so the blob's blocks
compress better and a presence bitmap is longer runs. Whether that holds at rung 6 is not known —
the signature space there is much larger — so the disk forecast below assumes no change in the
record blob, which makes it conservative.

## The pre-flight for rung 6

### Postings and the other files the terms add

`entities/terms/terms.u32` is exactly 4 bytes a pair, measured at both rungs. At 10,124,084,726
pairs it is **40.50 GB against 13.98 GB**, +26.51 GB. That is the largest single addition, and it
is arithmetic rather than a model.

`terms/postings.arrow` is 792,946 B at rung 6 today for 253 terms. The postings are in entity
order, which is signature order, so a term's posting is a few long runs rather than a scattered
set, and the file is far smaller than the pair count suggests. Modelled between **0.15 GB** (the
64-part file's 109.5 B a term, times 1,399,205 terms) and **2.8 GB** (the 64-part file's 0.272 B a
pair, times the rung 6 pairs). It is not what decides the disk either way.

`dictionary/terms-0.dict` is modelled at **18 MB** (13.0 B a term at 64 parts, times the rung 6
term count).

### The images

Taken from the rung 6 derivation the projection-build probe measured on 2026-09-16
(`git show probe/projection-build:probes/2026-09-14-projection-build/README.md` §"Question 1"),
which derived every year and species image over this rung's own permutation at the same keep rule
of 30 rows a container. Measured there, not here:

| | terms | kept | kept image bytes |
|---|---|---|---|
| `year` | 527 | 184 (34.9%) | 5,135.6 MB |
| `specieskey` | 1,398,425 | 88,794 (6.35%) | 6,061.1 MB |
| countries | 253 | 246 modelled | 11.4 MB |
| **total** | **1,399,205** | **89,224 modelled** | **11.21 GB** |

The table is 40.0 bytes a term, measured at 64 parts, so **56.0 MB** at 1,399,205 terms whatever
is kept. The one view's `.timg` file is modelled at **11.26 GB**.

⊘ The term-images memo (`docs/evidence/memos/2026-09-14-term-images.md` §2) gives the species total
as 5.2 GB where the probe's own table gives 6,061.1 MB, and the two cannot both be this rung at this
keep rule. The larger is used here, so the disk forecast does not depend on resolving it.

The derivation wall: the same probe measured `project` at 59 s for `year` and 98 s for
`specieskey`, and `run_optimize` at 2 s and 3 s, over this rung's permutation. The build's
`term_images` stage does that work and then serialises and writes 11.26 GB, so it is modelled at
**160 to 270 s**. Scaling the 64-part stage by rows would give 897 s; the direct rung 6
measurement of the same work is the better figure and the row scaling is not used.

### The build wall

§4d's third build (main `488e43e5`, 2026-09-14, 2 h 52 m 33 s) is the country-only measurement the
model corrects. Stages the access column does not touch are carried across unchanged; the stages it
touches are scaled by the 64-part ratio above, and where a stage is charged by pairs the per-pair
rate measured at 64 parts gives a second estimate. The two estimates are the bracket.

| stage | rung 6, country only | modelled, terms |
|---|---|---|
| `source_ids` | 81 s | 149 s |
| `dictionary` | 156 s | 1,760 to 2,470 s |
| `geometry_read` | 354 s | 940 to 1,330 s |
| batch loop, sorts | 180 s | 260 to 400 s |
| batch loop, assignments | 1,075 s | 1,490 s |
| `postings_write` | 59 s | 210 to 240 s |
| `term_images` | — | 160 to 270 s |
| every other stage | 7,763 s | 7,763 s |
| untimed, wall less stages | 685 s | 685 s |
| **wall** | **2 h 52 m 33 s** | **3 h 45 m to 4 h 07 m** |

Scaling the whole 64-part build by its own 1.44 ratio gives 4 h 08 m, which is the top of the
bracket. **Plan for about four hours.**

`dictionary` is the largest addition and the least certain. At 64 parts it rose 11.3× at a fixed
row count, and only 2.9× of that is the pair count: the rest is interning 186,179 distinct strings
instead of 253. Rung 6 interns 1,399,205, over a hash table 7.5× larger, so the per-pair rate there
is at least the 64-part rate and the upper end of the bracket is what to plan against.

`manifests`, the digest pass, ran at 3.04 GB/s over the bundle at the third build. The extra
38 GB adds about 12 s, which is inside the figures above.

### The bundle

| | bytes |
|---|---|
| rung 6 today, format 11 | 209,848,836,527 (195.4 GiB) |
| `terms.u32` | +26.51 GB |
| `.timg` | +11.26 GB |
| `postings.arrow` | +0.15 to +2.8 GB |
| `terms-0.dict` | +0.02 GB |
| `presence.roaring`, year and species | −0.23 GB |
| **modelled** | **247.6 to 250.2 GB (230.6 to 233.0 GiB)** |

Call it **248 GB, +18%**. If the record blob falls at rung 6 as it did at 64 parts it would be
11 GB smaller; that is not assumed.

### Disk

Measured on the box at the end of Part A: **135 GB free** (`df`; `statvfs` reports 144 GB, the
difference being the reserved blocks). `gbif-terms` took 66.52 GB for its own `points.parquet`; its
`members-taxonomy.parquet` is a hard link to `gbif`'s, so the 22.96 GB is counted once and will
survive `gbif` being removed.

**The rung 6 build cannot start from here.** The bundle is modelled at 248 GB and §4d measured that
the build's transient never exceeds the finished bundle (429 GB free at the start of the third
build, 221 GB at the minimum), so the build wants about 248 GB plus a margin. 135 GB is short by
about 110 GB.

What can be released, and what it costs:

| | frees | cost |
|---|---|---|
| a. `data/ladder/gbif/bundle` | 209.8 GB | the format 11 country-only rung 6 bundle. Rebuilding it is 2 h 53 m, and it is the bundle every §4d serve figure was taken on |
| b. a, and `data/ladder/gbif/points.parquet` | a + 55.05 GB | the country-only rung can then only be remade by reading the 258 GB share again, 66 min measured |
| c. a, after copying the bundle to the NAS | 209.8 GB | 52 min to write 209.8 GB at the measured 67 MB/s, and it is recoverable |
| — | — | `data/ladder/gbif/members-taxonomy.parquet` frees nothing: it is the same inode as `gbif-terms`'s |

Option (a) leaves 345 GB before the build and about 97 GB after it. Option (b) leaves 400 GB before
and about 152 GB after.

⊘ Free disk moved by ±70 GB during Part A from other sessions' cargo target directories (141 GB
across the worktrees at one point, 73 GB after one was cleaned). A 248 GB build wants (a)'s margin
rather than anything tighter.

## Files

- `build-gbif-64p-base.log`, `build-gbif-64p-terms.log`: the two 64-part builds.
- `rewrite-gbif-terms.log`: the rung 6 `points.parquet` rewrite.
- `cargo-build-release.log`: the binary the builds ran.
- `results.json`: every figure above, measured and modelled marked.

## Where this stops

Part A ends here, as ruling D asks. The rung 6 build is not started: the controller confirms the
figures above and the owner decides where the old rung 6 bundle goes. Parts B and C, the route
measurement and the chooser's constants, follow the build.

## Part B at the 64-part rung: the four routes

Status: measured 2026-09-17 on `data/ladder/gbif-64p-terms/bundle` at **bundle format 13**,
commit `3993133a`, which is this branch after main's whole-piece review fixes and the
derivation window fix merged.
25,846,007 rows, one segment, no extents, 186,179 dictionary terms, an 84,052,461-byte image file
at header version 2. The rung was rebuilt from scratch for this: the earlier bundle was format 12
and is refused. Uncapped — the bundle holds 1.6 GiB and the cap exists for the whole-corpus one,
which nothing here opened. `nice -n 10`, under `setsid`.

`crates/tessera-bench/src/bin/route_probe.rs` is a new binary rather than an extension of the
parked `projection_probe`. That probe hand-rolls each route to size the structure the design
needed; this one asks a different question — what the **shipped** chooser and the shipped routes
cost — so every arm is `RowProjection::new(&inputs, &row_space)` with `ProjectionInputs::force`
set, the one constructor the request path uses, over a fragment built through
`FragmentCache::get_or_build`. A probe that re-implemented a route would measure the
transcription.

### The principals

Ten, not eleven: the five compartment ladder rungs, `all`, 300 years drawn uniformly, and species
drawn size-weighted at 1,000, 10,000 and 100,000. The eleventh is the one the brief leaves to the
controller after the term census, and is not drawn here.

Seed **20260917**, and both draws read the bundle's own dictionary so the same invocation runs
at either rung: years uniformly over the terms prefixed `y:`, species weighted by the posting rows
the image table records, both without replacement by the exponential race (one pass and an
N-sized heap, which is what makes it usable over the whole corpus's 1.4×10⁶ species terms).

⊘ The compartment ladder is **composed from the rung's own `country-ranks.json`** by
`serve_battery.py`'s `compose_ladder` rule rather than written down. The memo's rung 6 sets name
`XZ`, which the 64-part prefix's dictionary does not hold, and a probe that dropped an unknown
term would report a smaller principal than the one asked for. Composing gives the memo's sets at
rung 6 and this rung's equivalents here.

### The table

Milliseconds, cold / warm. A cold run follows `posix_fadvise(POSIX_FADV_DONTNEED)` over
`permutation.bin`, the `.timg` file and `postings.arrow`; a warm run is the same arm again. All
measured.

| principal | terms | coverage | walk cold / warm | split cold / warm | complement cold / warm | chooser cold / warm | route taken | fastest forced | margin |
|---|---|---|---|---|---|---|---|---|---|
| `p1` | 4 | 1.0% | 2.4 / 2.4 | 0.3 / 0.3 | 150.5 / 183.6 | 10.2 / 0.3 | `split` | `split` | +4% |
| `p5` | 6 | 5.0% | 8.3 / 7.7 | 0.3 / 0.3 | 162.0 / 136.3 | 12.0 / 0.3 | `split` | `split` | -3% |
| `p10` | 5 | 10.0% | 15.5 / 14.9 | 0.4 / 0.4 | 132.3 / 130.3 | 4.7 / 0.4 | `split` | `split` | +9% |
| `p25` | 7 | 25.0% | 40.7 / 39.2 | 0.4 / 0.4 | 115.1 / 108.8 | 15.5 / 0.4 | `split` | `split` | +0% |
| `p50` | 7 | 50.0% | 80.6 / 79.7 | 0.7 / 0.7 | 100.0 / 96.2 | 14.4 / 0.9 | `split` | `split` | +30% |
| `all` | 252 | 100.0% | 0.2 / 0.1 | 37.2 / 2.3 | 0.7 / 0.5 | 0.1 / 0.1 | `whole_domain` | `walk` | -12% |
| `year300` | 300 | 47.2% | 81.2 / 72.9 | 12.9 / 12.8 | 82.5 / 78.2 | 38.2 / 13.5 | `split` | `split` | +6% |
| `species1000` | 1,000 | 51.7% | 78.9 / 79.4 | 25.8 / 25.2 | 84.1 / 80.7 | 47.0 / 24.5 | `split` | `split` | -3% |
| `species10000` | 10,000 | 81.3% | 138.5 / 137.7 | 72.9 / 70.3 | 42.4 / 43.6 | 45.8 / 39.3 | `complement` | `complement` | -10% |
| `species100000` | 100,000 | 92.0% | 126.8 / 132.8 | 122.3 / 121.2 | 25.1 / 24.5 | 25.5 / 23.0 | `complement` | `complement` | -6% |

**Every arm's rows equal the walk's, for all ten principals** — checked by symmetric difference
per arm, and the probe exits non-zero if any differ. Cold and warm agree too.

**The chooser takes the fastest forced route for every principal**, and takes the same route it
took at format 12: the split from 1% to 52% coverage, the complement above 81%, the whole-domain
short-circuit for the principal holding every term. The margin column is the chosen route against
the fastest forced one; it runs −12% to +30%, wider than the format 12 run's −11% to +6% and for
the same reason — the arms it is a ratio of are 0.3 to 0.9 ms at the compartment ladder, so a
tenth of a millisecond of scheduling is tens of per cent. The chosen and fastest routes are the
same route in every row, which is the statement that does not depend on the noise.

On `all` the chooser answers `whole_domain`, which is neither of the three the offline `choose`
prices: `RowProjection::new` checks the whole-domain short-circuit before it prices anything, so a
grant covering the domain never reaches the chooser. The offline verdict recorded beside it says
`complement`, and that is not a miss.

⊘ **The cold arms are barely cold.** `read_bytes` is 0 and the major-fault delta is 0 on every
run: `posix_fadvise(DONTNEED)` does not reach a page this process holds mapped, and the
permutation and the images are mapped for the probe's whole life. What the cold column shows at
this rung is a first touch of pages the previous arm left resident — the split's 10.2 ms against
its 0.3 ms repeat is croaring's own warm-up, not disk. A cold reading that means disk needs a rung
whose files do not fit the page cache.

### What the chooser was given

| principal | held | kept images | array+run | bitset | residual entities | walk ns | split ns | complement ns | chosen |
|---|---|---|---|---|---|---|---|---|---|
| `p1` | 258,459 | 4 | 53 | 0 | 0 | 2 | 0 | 281 | `split` |
| `p5` | 1,292,304 | 4 | 34 | 0 | 14 | 8 | 0 | 270 | `split` |
| `p10` | 2,584,596 | 2 | 53 | 0 | 1,670 | 17 | 0 | 256 | `split` |
| `p25` | 6,461,499 | 6 | 159 | 0 | 3 | 42 | 0 | 213 | `split` |
| `p50` | 12,923,003 | 6 | 278 | 0 | 3 | 84 | 0 | 142 | `split` |
| `all` | 25,846,007 | 246 | 1,252 | 0 | 1,716 | 168 | 0 | 0 | `whole_domain` |
| `year300` | 12,186,941 | 64 | 21,368 | 333 | 53,155 | 79 | 8 | 150 | `split` |
| `species1000` | 13,355,838 | 642 | 68,488 | 48 | 343,917 | 87 | 28 | 137 | `split` |
| `species10000` | 21,017,298 | 3,095 | 136,501 | 58 | 2,711,579 | 137 | 78 | 53 | `complement` |
| `species100000` | 23,786,088 | 5,162 | 145,633 | 58 | 4,981,644 | 155 | 106 | 23 | `complement` |

Milliseconds of modelled cost, from `ROUTE_COSTS` as it stands. The split's advantage at the
compartment ladder is the shape the design predicted: a country's rows are long runs in Morton
order, so `p50` unions 278 containers where the walk crosses 12.9 million entities.

### The residual, now priced per entity

The review changed the residual term from rows to entities and had the image table record each
posting's cardinality to supply it (header version 2). **At this rung the two are the same
number**, for every principal:

Residual, the old row overcount against the new entity count:

The base permutation is a bijection onto its rows here — `bound`, `base_rows` and `total_rows` are
all 25,846,007 and there are no extents — so every held entity has exactly one row and a posting's
cardinality is its image's. The change is right where a permutation drops entities or a view
carries extents; **the 64-part rung cannot demonstrate it**, and this table is the evidence that it
is a no-op here rather than evidence that it does nothing.

## Pass 2b, measured on a fold of the 64-part rung

One fold requested through `POST /control/compact` on the rung's own control port, the staircase
read from `/control/status`. Server stopped by pid afterwards. Measured.

| | |
|---|---|
| whole fold | **54.1 s** (`last_secs` 54, staircase max RSS 1.65 GB) |
| **pass 2b, term images** | **1.377 s**, 2.5% of the fold |
| the derivation inside it | 1.304 s |
| kept | 5,516 of 186,179 terms |
| payload / table | 76,605,165 B / 7,447,160 B |
| `.timg` on disk | 84,052,461 B |

**The fold reproduced the build's image file exactly**: same kept count, same payload, same table,
and the same 84,052,461 bytes on disk as the build wrote. That is decision 0139's one
implementation, confirmed on a real corpus rather than on a fixture. The file is the same size at
format 13 as at format 12 — header version 2 reinterprets the table's existing per-term field as
the posting's cardinality rather than adding one, so an entry is still 40 bytes.

The staircase, seconds: `1 row space` 4.68, `2 postings` 1.53, **`2b term images` 1.38**,
`4a attributes` 5.70, `4c entity terms` 25.92, `5 digests + fsync` 1.60, `7 memberships` 0.55,
`8 derived` 10.10, `12 retire` 0.26, `13 open` 0.09, `14 adopt` 0.19, `15 warm` 1.92,
`17 reclaim` 0.14. `4c entity terms` is 48% of the fold and pass 2b is 2.5% of it.

### What the format 13 rebuild changed, and what the window fix changed after it

Three readings of the build's `term_images` stage over the same corpus on this box, at four
workers throughout except the first. All measured.

| | build stage | note |
|---|---|---|
| format 12, twelve workers | 6.63 s | before the worker cap |
| format 13, four workers | 12.00 s | the cap alone: **1.8× slower than twelve** |
| format 13, four workers, window fix | **3.65 s** | **3.3× faster than the cap alone, 1.8× faster than twelve** |

⊘ The first two were taken before the derivation window fix and are superseded by the third. They
are kept because the middle one is what showed the cap was costing rather than saving, and the
fix is what that reading led to.

The fix makes a window the `threads` postings that **will be projected**, so a term too small to
be kept never enters the parallel work. At this rung 180,663 of the 186,179 terms are in that
class, which is why the stage was spending its time dispatching work that did nothing: at four
workers the cap had made the dispatch four windows deep instead of twelve, and the dispatch was
the cost. The bytes are identical.

Against the rest of the piece, unchanged by either:

| | format 12 | format 13, window fix | |
|---|---|---|---|
| build `term_images` stage | 6.63 s | **3.65 s** | −45% |
| fold pass 2b | 1.484 s | 1.377 s | −7% |
| whole fold | 51.4 s | 54.1 s | +5% |
| kept / payload / table / `.timg` | 5,516 / 76,605,165 / 7,447,160 / 84,052,461 | identical | — |
| routes chosen, all ten principals | split, complement, whole_domain | identical | — |

The build's stage is now 2.7× the fold's pass 2b rather than 8.7×, which is the ratio a
four-worker build against a single-threaded fold should be near when neither is paying for
dispatch it does not need. The fold's pass was never affected: it derives the same images and
writes the same bytes, and its 1.38 s is the same before and after.

### Rung 6, modelled

Scaling pass 2b by rows (135.25×) gives **186 s**. The Part A pre-flight modelled the build's
`term_images` stage at 160 to 270 s from the projection-build probe's own rung 6 derivation, by a
different route, and the two agree — which is the cross-check that scaling was worth making, not a
second measurement. Both are modelled; neither has been run at rung 6. The build's stage is 2.7× the fold's pass at this rung, which is inside the
bracket's own spread; the reading that put it at 8.7× was the dispatch the window fix removed.

Scaling the whole fold by rows gives 2.0 h, which is **not** reported as a rung 6 fold estimate:
`4c entity terms` and `8 derived` are two thirds of the fold here and neither is linear in rows.

## The rung 6 build is refused by the entity-terms transpose, and the corpus cannot be built as ruled

Status: measured 2026-09-17, commit `3993133a` (this branch after main's `b08413f1`). The build ran
under `systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=2G`, `nice -n 10`, and failed
after **1 h 08 m 47 s** with:

```
build FAILED: invalid input: entity-terms transpose: invalid entity-terms transpose at
.../entities/terms/terms.u32: the layer's term count passes u32::MAX at entity 1531658054
```

**This is a format ceiling, not a resource failure.** `entities/terms/offsets.u32` is one `u32`
start offset per entity into `terms.u32` (`tessera_store::entity_terms`, contracts §2.4), so a
partition's entity-terms layer holds at most **4,294,967,295 pairs**. Disk was never a constraint:
340 GB was free when it stopped, the partial prefix was swept as designed, and the memory cap was
never approached (the largest stage peak was 17.9 GiB of a 24 GiB cap).

| | pairs | against the ceiling |
|---|---|---|
| rung 6 as built today, country only | 3,495,729,729 | 81.4% — fits |
| **ruling D, country + year + species** | **10,124,084,726** | **2.36× — refused** |

At 3,495,729,729 rows the ceiling allows **1.229 terms a row**. Ruling D asks for 2.896. Measured
from the corpus census, any **two** of the three term classes also overflow:

| access column | pairs | |
|---|---|---|
| country alone | 3,495,729,729 | fits |
| year alone | 3,407,218,226 | fits |
| species alone | 3,221,136,771 | fits |
| country + year | 6,902,947,955 | refused |
| country + species | 6,716,866,500 | refused |
| year + species | 6,628,354,997 | refused |
| country + year + species | 10,124,084,726 | refused |

All three classes together fit up to **42.4% of the corpus, 1,483,002,688 rows**, which is where
the failure landed: entity 1,531,658,054, 43.8% in.

### What it cost, and what the stages said before it stopped

The stages that ran are worth keeping: they are the first measurement of what the access column
costs at this scale, and they correct the Part A pre-flight.

| stage | rung 6, country only (§4d) | ruling D, measured | ratio | the pre-flight's model |
|---|---|---|---|---|
| `source_ids` | 81 s | 84.1 s | 1.04 | 149 s |
| `dictionary` | 156 s | **1,481.4 s** | 9.50 | 1,760 to 2,470 s |
| `geometry_read` | 354 s | **1,359.4 s** | 3.84 | 940 to 1,330 s |
| `pairs_pack` | — | 0.9 s | — | — |
| batch loop, 8 of 15 batches | — | sorts 226.7 s, assignments 471.6 s | — | sorts 260 to 400 s, assignments 1,490 s (15 batches) |

The pre-flight **over**-forecast `dictionary` by 16 to 40% and **under**-forecast `geometry_read` by
2 to 31%. Both were scaled from the 64-part rung's ratios, and both are within the spread that
scaling deserves. The batch loop is 15 batches of 234,881,024 items rather than §4d's ten, because a
batch is bounded by pairs and there are 2.9× more of them.

⊘ The build's own disk forecast printed **597.8 GB at peak against 364.5 GB available** and went on,
as it is designed to. It was never tested: the transpose refused first. §4d records the same
forecast overstating by more than twice.

### This needs an owner ruling

**Resolved 2026-09-18 by option (a), and at a twentieth of the cost priced below**: the offsets are
paged rather than widened, so `offsets.u32` stays 14.0 GB and `bases.u64` is 426,728 B. The build
is measured under "Rung 6 built", further down.

Ruling D's corpus cannot be built at rung 6 as it stands. The options, with what each costs:

- **a. Widen the transpose's offsets to `u64`.** A bundle format bump and a reader change in
  `entity_terms`. `offsets.u64` at rung 6 is 28.0 GB against 14.0 GB, so the bundle grows about
  14 GB beyond the Part A model's 248 GB. This is the only option that keeps ruling D's corpus.
- **b. One term class a row instead of three.** Any single class fits. It abandons what ruling D
  asked for: a principal shaped like a user's term set needs the species class beside the
  compartment.
- **c. Measure at 42% of the corpus**, 1.48×10⁹ rows, keeping all three classes and the whole
  dictionary shape. A billion-row rung is still a billion-row rung, and this is the only option
  that needs no code change. It is not rung 6.
- **d. Leave the measurement at the 64-part rung**, which is built, measured and complete in this
  directory. It says nothing about the disk-bound behaviour a 3.5×10⁹-row permutation has.

The 64-part rung's Part B and pass 2b figures above stand: they were taken on a built bundle and
nothing here changes them.

---

## The ceiling is lifted, and rung 6 is built

Status: measured 2026-09-18, this branch at `34c174b5` (main `0dc6b4cb` merged). The transpose's
offsets are paged — one `u32` offset a row against a `u64` base a page, `entities/terms/bases.u64`
— so a partition's pairs are no longer capped at 2^32 (main `5e0853ad`, **bundle format 14**).
Option (a) of the table above, at a twentieth of the cost it was priced at: `bases.u64` is
**426,728 B** at rung 6, not the 14.0 GB a `u64` offset a row would have cost. Everything below is
at format 14.

### The 64-part rung, rebuilt

`data/ladder/gbif-64p-terms` rebuilt from scratch at format 14 (`bundle`, `.tessera` cleared;
`tessera build --memory-budget 24g --no-oracle-pairs --stage-timings`, `nice -n 10`, uncapped).
Measured. The bundle is **1,662,298,604 B** against format 13's 1,662,295,444 — **+3,160 B, which
is `bases.u64` exactly** and nothing else. The images are byte-identical: 5,516 kept of 186,179,
76,605,165 B of payload, 7,447,160 B of table, an 84,052,461-byte `.timg`. The `term_images` stage
took **3.28 s** against format 13's 3.65 s, inside this box's spread.

The route probe re-run on it takes the same route for every principal as at format 13 — the split
from 1% to 52% coverage, the complement at 81% and 92%, the whole-domain short-circuit for `all` —
and every arm's rows equal the walk's. Milliseconds, cold / warm, measured:

| principal | terms | coverage | walk | split | complement | chooser | route taken |
|---|---|---|---|---|---|---|---|
| `p1` | 4 | 1.0% | 2.6 / 2.4 | 0.4 / 0.3 | 171.1 / 185.4 | 13.0 / 0.3 | `split` |
| `p5` | 6 | 5.0% | 8.0 / 8.4 | 0.3 / 0.3 | 161.2 / 140.8 | 11.0 / 0.4 | `split` |
| `p10` | 5 | 10.0% | 16.6 / 15.6 | 0.4 / 0.4 | 135.6 / 132.1 | 4.3 / 0.5 | `split` |
| `p25` | 7 | 25.0% | 40.7 / 39.5 | 0.5 / 0.4 | 123.5 / 121.2 | 14.3 / 0.4 | `split` |
| `p50` | 7 | 50.0% | 94.4 / 78.4 | 0.7 / 0.8 | 82.0 / 84.7 | 12.2 / 0.7 | `split` |
| `all` | 252 | 100.0% | 0.2 / 0.2 | 34.9 / 2.0 | 0.6 / 0.5 | 0.1 / 0.1 | `whole_domain` |
| `year300` | 300 | 47.2% | 84.6 / 74.4 | 13.7 / 12.9 | 87.4 / 77.9 | 41.1 / 13.4 | `split` |
| `species1000` | 1,000 | 51.7% | 96.5 / 77.9 | 27.6 / 27.3 | 92.1 / 93.2 | 51.3 / 25.8 | `split` |
| `species10000` | 10,000 | 81.3% | 121.6 / 125.6 | 76.0 / 67.0 | 42.5 / 40.6 | 42.6 / 40.0 | `complement` |
| `species100000` | 100,000 | 92.0% | 130.9 / 133.6 | 121.7 / 120.2 | 26.4 / 23.7 | 23.2 / 22.2 | `complement` |

Results in `gbif-64p-terms-f14/`.

### The fold at format 14

One fold requested through `POST /control/compact`, the staircase read from `/control/status`,
the server stopped by pid. Measured, seconds: `1 row space` 8.39, `2 postings` 2.71,
**`2b term images` 2.38**, `4a attributes` 8.62, `4c entity terms` 23.46, `5 digests + fsync` 0.86,
`7 memberships` 0.58, `8 derived` 10.87, `12 retire` 0.30, `13 open` 0.09, `14 adopt` 0.18,
`15 warm` 2.09, `17 reclaim` 0.13; whole fold **60 s**, staircase max RSS 1.66 GB.

**The fold reproduced the build's files exactly again**, now including the paged transpose:
kept 5,516, payload 76,605,165 B, table 7,447,160 B, `.timg` 84,052,461 B, and `terms.u32`,
`offsets.u32` and `bases.u64` at the same 300,093,416 / 103,384,032 / 3,160 bytes the build wrote.
That is decision 0139's one implementation holding across the format change.

⊘ Pass 2b reads 2.38 s here against format 13's 1.38 s and the whole fold 60 s against 54 s. Other
sessions' cargo runs shared the box during this fold and did not during that one; the ratio between
the two passes is unchanged, so this is read as the box, not as the format. Not isolated.

## Rung 6 built: the whole corpus with three term classes a row

Measured 2026-09-18. `tessera build --memory-budget 24g --no-oracle-pairs --stage-timings` under
`systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G`, `nice -n 10`, under
`setsid`, on `34c174b5`. 3,495,729,729 rows, 1,399,206 terms, **10,124,084,726 pairs** — 2.36× the
ceiling that refused this build the day before. Exit 0 in **3 h 46 m 42 s**, at the bottom of the
pre-flight's 3 h 45 m to 4 h 07 m bracket. Log `build-rung6-f14.log`.

| stage | §4d third build, country only | the pre-flight's model | **measured, terms** |
|---|---|---|---|
| `source_ids` | 81 s | 149 s | **89.6 s** |
| `dictionary` | 156 s | 1,760 to 2,470 s | **1,276.3 s** |
| `geometry_read` | 354 s | 940 to 1,330 s | **1,281.6 s** |
| `pairs_pack` | — | — | **0.3 s** |
| batch loop, sorts | 180 s (10 batches) | 260 to 400 s | **623.1 s** (15 batches) |
| batch loop, assignments | 1,075 s | 1,490 s | **1,526.3 s** |
| `postings_write` | 59 s | 210 to 240 s | **194.5 s** |
| `attribute_tail` | 1,331 s | carried | **1,491.6 s** |
| `layers` | 1,881 s | carried | **1,842.7 s** |
| `filter_postings` | 1,076 s | carried | **1,019.3 s** |
| `record_blob` | 806 s | carried | **755.0 s** |
| `tiler_sort` | 208 s | carried | **257.7 s** |
| `segment_write` | 577 s | carried | **619.9 s** |
| `artifact_pass` (two passes) | 1,815 s | carried | **1,717.6 s** |
| `term_images` | — | 160 to 270 s | **251.2 s** |
| `manifests` | 69 s | ~81 s | **62.9 s** |
| sum of stages | 9,668 s | — | **13,009.5 s** |
| untimed, wall less stages | 685 s | 685 s | **592.4 s** |
| **wall** | **2 h 52 m 33 s** | **3 h 45 m to 4 h 07 m** | **3 h 46 m 42 s** |

**The eight stages the access column does not touch cost 7,766.5 s against §4d's 7,763 s** — the
pre-flight carried them across unchanged and was right to within 0.05%. The whole of the difference
between the two builds is in the six stages the column does touch, plus the new one.

Where the pre-flight was wrong, and by how much: it **over**-forecast `dictionary` by 38 to 94%
(1,276 s measured against 1,760 to 2,470 s modelled) and **under**-forecast the batch loop's sorts
by 56 to 140% (623 s against 260 to 400 s). `geometry_read`, `postings_write`, `term_images` and
the assignments all landed inside their brackets, the assignments to 2.4%. The refused build of
2026-09-17 had already measured `dictionary` at 1,481 s and `geometry_read` at 1,359 s on a busier
box; both came in lower here. The wall landed inside its bracket, which is what the pre-flight was
for.

Peak: the stage timings print `VmHWM`, which includes mapped file pages, and it ended at
**24,190 MiB** — the cap itself, reached in `tiler_sort`. The largest *anonymous* figure the build
prints is its own entity-order forecast, **20,094 MiB against the 24,576 MiB budget**, and that
figure is a lower bound by its own statement. No anonymous peak was sampled for this run; the
comparable §4d figure is 12.1 GB and is not comparable here, because the term images hold up to
5,000 MiB of it by the same forecast. Assumed inside the cap because `oom_kill` never fired and the
build completed; not measured.

### The images

From the build report line (ruling G), one view `geo`, measured:

| | modelled in the pre-flight | **measured** |
|---|---|---|
| terms | 1,399,205 | **1,399,206** |
| kept | 89,224 | **21,894 (1.6%)** |
| payload | 11.21 GB | **10.26 GB** |
| table | 56.0 MB | **55.97 MB** |
| largest image | — | **345.23 MB** |
| derivation wall | 160 to 270 s | **251.2 s** |
| `.timg` on disk | 11.26 GB | **10,314,190,823 B** |

The table is 40 bytes a term whatever is kept, and 40 × 1,399,206 = 55,968,240 — the model was
arithmetic and the measurement matches it. **The kept count is a quarter of the model's**: the
pre-flight took it from the projection-build probe's rung 6 derivation of every year and species
term at the same keep rule, and this build keeps 21,894 where that predicted 89,224. The payload is
nonetheless within 9% of the model, so the terms that were dropped are the small ones; that is the
keep rule working as designed and the discrepancy is in the count, not the bytes. ⊘ Not run down:
the two derivations are at different commits and the probe's own figure was never re-taken.

### The bundle

**226,569,780,565 B (211.0 GiB)**, measured, against the pre-flight's 247.6 to 250.2 GB. The model
was **21.0 GB high**, and it said it would be: it assumed the record blob does not shrink, where
the 64-part rung had measured it shrinking by 92 MB on the same change.

| file | rung 6, country only (§4d) | **terms** | change |
|---|---|---|---|
| `entities/terms/terms.u32` | 13,982,918,916 | **40,496,338,904** | +26.51 GB |
| `entities/terms/offsets.u32` | 13,982,918,920 | **13,982,918,920** | — |
| `entities/terms/bases.u64` | — | **426,728** | +0.43 MB |
| `term-images/….timg` | ~102 kB | **10,314,190,823** | +10.31 GB |
| `terms/postings.arrow` | 792,946 | **912,644,914** | +0.91 GB |
| `dictionary/terms-0.dict` | ~13 kB | **18,329,973** | +0.02 GB |
| **the whole bundle** | 209,848,836,527 | **226,569,780,565** | **+16.72 GB, +8.0%** |

The additions above sum to **+37.75 GB** and the bundle grew by **16.72 GB**, so about **21.0 GB
of the rest of it shrank**. That is the size the pre-flight left out, and it is the same sign and
about 230× the size of the 64-part rung's 92 MB — measured by difference, not attributed to a file,
because §4d's bundle was released to make room for this one and cannot be diffed against.

`terms.u32` is 4 bytes a pair exactly: 4 × 10,124,084,726 = 40,496,338,904. `postings.arrow` landed
at 0.91 GB, inside the pre-flight's 0.15 to 2.8 GB bracket and near its 0.090 B-a-pair midpoint.
`terms-0.dict` was modelled at 18 MB and measured 18.33 MB.

**Disk**: 321 GB free at the start, **186 GB free** at the end. The build's own forecast printed
597.8 GB at peak against 343.6 GB available and went on, as it is designed to; the transient never
exceeded the finished bundle, as §4d also found. The forecast overstated by more than three times.

## Part B at rung 6: the four routes over 3.5 billion rows

Measured 2026-09-18 on `data/ladder/gbif-terms/bundle` at format 14, commit `34c174b5`, under
`systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G` and `nice -n 10`, under
`setsid`. 3,495,729,729 rows, one segment, no extents, 1,399,206 dictionary terms, a
10,314,190,823-byte image file. Seed **20260917**, the same one the 64-part run used, and the same
`run.sh` line with one principal added. Results in `gbif-terms/`.

### Open

`open_bundle` with digests verified: **99.73 s wall, 227.52 s CPU, 225,106,968,576 bytes read** —
99.4% of the bundle, which is what verifying every file's digest at open means. Measured.

⊘ §4d has no bare `open_bundle` figure to compare with: its open figures are `tessera serve` to
`/readyz`, 197 s and 202 s, which builds the artifact row forms as well. The images' share of the
99.73 s is **not separable here**: a rung 6 bundle at format 11 cannot be opened at 14, so the only
way to take the difference would be to rebuild without the access column. Modelled from the file
sizes at the digest pass's measured 3.04 GB/s (§4d): the `.timg` is 10.31 GB, so about **3.4 s**,
3.4% of the open.

### The eleven principals

The eleventh is **`species3000`**, size-weighted like the others. The ten of the memo leave a decade
between `species1000` and `species10000`, and that decade is where the split gives way to the
complement — at 64 parts the crossover is between 51.7% and 81.3% coverage and nothing was measured
inside it. `species3000` lands at **65.7%** coverage and is the only principal in the table whose
route is not obvious from either neighbour. It takes the split, and the split is 2.0× faster than
the complement there, which puts the crossover above it.

Seconds, cold / warm. All measured.

| principal | terms | coverage | walk | split | complement | chooser | route taken | fastest forced |
|---|---|---|---|---|---|---|---|---|
| `p1` | 5 | 1.0% | 0.505 / 0.364 | 0.002 / 0.019 | 52.96 / 66.60 | 0.042 / 0.002 | `split` | `split` |
| `p5` | 6 | 5.0% | 1.528 / 0.991 | 0.005 / 0.003 | 45.49 / 41.75 | 0.033 / 0.003 | `split` | `split` |
| `p10` | 7 | 10.0% | 1.995 / 2.025 | 0.009 / 0.007 | 38.83 / 39.34 | 0.045 / 0.010 | `split` | `split` |
| `p25` | 7 | 25.0% | 7.454 / 7.400 | 0.027 / 0.021 | 31.28 / 30.67 | 0.052 / 0.021 | `split` | `split` |
| `p50` | 8 | 50.0% | 19.25 / 19.16 | 0.038 / 0.032 | 20.96 / 20.66 | 0.052 / 0.030 | `split` | `split` |
| `all` | 253 | 100.0% | 0.022 / 0.016 | 0.606 / 0.245 | 0.055 / 0.058 | 0.015 / 0.015 | `whole_domain` | `walk` |
| `year300` | 300 | 47.0% | 21.72 / 21.64 | 2.143 / 2.126 | 24.63 / 25.45 | 3.449 / 2.118 | `split` | `split` |
| `species1000` | 1,000 | 50.0% | 25.89 / 31.28 | 5.363 / 5.213 | 24.43 / 23.40 | 11.90 / 5.069 | `split` | `split` |
| `species3000` | 3,000 | 65.7% | 30.62 / 30.05 | 8.319 / 8.633 | 16.24 / 16.46 | 10.49 / 8.098 | `split` | `split` |
| `species10000` | 10,000 | 78.6% | 35.69 / 36.83 | 12.50 / 12.56 | 10.84 / 10.72 | 10.81 / 11.10 | `complement` | `complement` |
| `species100000` | 100,000 | 89.8% | 41.36 / 46.74 | 23.46 / 21.90 | 5.461 / 5.209 | 5.702 / 5.357 | `complement` | `complement` |

**Every arm's rows equal the walk's, for all eleven principals** — checked by symmetric difference
per arm, cold against warm as well, and the probe exits non-zero if any differ. It exited 0.

**The chooser takes the fastest forced route for every principal.** The split holds from 1% to
65.7% coverage, the complement from 78.6%, and `all` never reaches the chooser: `RowProjection::new`
answers a grant covering the domain from the whole-domain short-circuit before it prices anything.
The offline verdict recorded beside `all` is `complement`, and that is not a miss.

**The split is worth 2.1 to 591× the walk at this rung**, warm. At `p50` it is **591× faster**:
32 ms against 19.2 s, because a country's rows are long runs in Morton order and the split unions
32,359 containers where the walk crosses 1.75 billion entities. The ratio falls as the term set
widens — 10.2× at `year300`, 2.1× at `species100000` — and it is where it falls below the
complement's that the chooser changes route. At `species100000` the complement is **9.0× the
walk**, 5.2 s against 46.7 s.

The chooser arm against the fastest forced arm of the same route, warm: **−6.2% to +3.5%** on the
five principals whose fastest arm is over 100 ms, and −91% to +38% on the six whose fastest arm is
2 to 32 ms, where a few milliseconds of scheduling is tens of per cent. The chosen route and the fastest route are the same route in every row, which is the
statement that does not depend on the noise.

### The cold arms are cold at this rung

Unlike at 64 parts, `posix_fadvise(POSIX_FADV_DONTNEED)` takes here: the permutation is 13.98 GB,
the images 10.31 GB and the postings 0.91 GB against a 24 GiB cap, so the pages are not all
resident to begin with. Measured, cold run against warm run of the same arm: `p5`'s walk read
**687 MB** cold and 0 warm with 232 major faults against 0; `species1000`'s chooser read **5.10 GB**
cold and 0 warm, 976 major faults against 0; `year300`'s chooser **2.85 GB** against 0. Seventeen
of the 44 cold runs still read 0 bytes.

⊘ **A cold arm is only cold for the file no earlier arm has touched.** The arms run walk, chooser,
split, complement, and the chooser and split take the same route at nine of the eleven principals,
so the split arm finds the images the chooser arm just read: every split arm's cold `read_bytes` is
0 except `all`'s. The cold column is a first touch, not a cold start, and the honest cold figure
for a route is the first arm that takes it. The eviction is best effort by construction and
`read_bytes` is what says whether it took.

Anonymous peak over both runs of an arm, measured: 232 MB at `p1`'s split, 2,527 MB at
`species100000`'s split, and the complement between 643 MB and 2,140 MB. The largest figure in the
table is the split's at 100,000 terms; the whole probe stayed inside the 24 GiB cap.

### Pass 2b at rung 6, modelled

Not measured: no fold was run on the rung 6 bundle. Scaling the 64-part fold's pass 2b by rows
(135.25×) gives **322 s** from this format 14 fold, or 186 s from the format 13 one. The build's own
`term_images` stage at rung 6 is **251.2 s measured**, between the two, and the build's stage is the
figure to use — it did the work at this rung, where both of those are extrapolations of a rung 135
times smaller. The whole fold is not modelled at rung 6: `4c entity terms` and `8 derived` are two
thirds of the 64-part fold and neither is linear in rows.

## Part C: the chooser's constants, re-derived from rung 6

The five rates were modelled on 2026-09-16 from the memo's principals. They are now least squares
over the eleven warm forced-route arms above: the walk and complement through the origin against
held and outside entities, and the three split rates as one non-negative least squares over array
and run containers, bitset containers and residual entities. `all` is left out of the walk fit —
the whole-grant short-circuit answers it in 16 ms without walking.

| rate | before (modelled 2026-09-16) | **after (measured 2026-09-18)** | |
|---|---|---|---|
| `walk_ns_per_entity` | 6.5 | **13.8** | 2.1× |
| `split_ns_per_array_or_run` | 350 | **430** | 1.2× |
| `split_ns_per_bitset` | 1,000 | **12,000** | 12× |
| `residual_ns_per_entity` | 11 | **24** | 2.2× |
| `complement_ns_per_entity` | 11 | **14.1** | 1.3× |

Per-principal rates behind the two single-rate fits, ns, measured: the walk runs 5.7 to 17.9 (5.7
to 11.0 over the compartment ladder, 13.1 to 17.9 over the term-set principals) and the complement
12.5 to 14.6, with `p1`'s 19.2 the one outlier — its complement arm reads 66.6 s warm against 53.0 s
cold, and it is the noisiest cell in the table. A rung whose permutation does not fit the cap prices
the walk at twice the rate the 10⁷-row model assumed, which is the whole of the walk's 2.1×.

**The new constants change no verdict, at either rung.**

| principal | old constants | new constants | route run | fastest forced | margin |
|---|---|---|---|---|---|
| `p1` | `split` | `split` | `split` | `split` | −91% |
| `p5` | `split` | `split` | `split` | `split` | +7% |
| `p10` | `split` | `split` | `split` | `split` | +38% |
| `p25` | `split` | `split` | `split` | `split` | −1% |
| `p50` | `split` | `split` | `split` | `split` | −8% |
| `all` | `complement` | `complement` | `whole_domain` | `walk` | −6% |
| `year300` | `split` | `split` | `split` | `split` | −0.4% |
| `species1000` | `split` | `split` | `split` | `split` | −3% |
| `species3000` | `split` | `split` | `split` | `split` | −6% |
| `species10000` | `complement` | `complement` | `complement` | `complement` | +3.5% |
| `species100000` | `complement` | `complement` | `complement` | `complement` | +2.8% |

The same re-run over the 64-part rung's recorded inputs changes no verdict there either. The margin
column is the chooser arm's warm wall against the fastest forced arm's, so it measures the arms'
own scatter, not the choice: **the chosen route is the fastest forced route in every row of both
tables, so the margin on the choice is 0%**, and the stated margin on the wall is **−6.2% to +3.5%
where the fastest arm is over 100 ms**.

That the constants moved by up to 12× and moved no decision is the finding. The three routes are
separated by an order of magnitude or more wherever the chooser has to decide — the closest call in
the table is `species10000`, where the complement is 10.72 s and the split 12.56 s, 17% apart — so
the chooser is not sensitive to rates of this accuracy. The re-derivation is worth having because
the next route, or a rung with extents, may land in a place where it is.

⊘ `split_ns_per_bitset`'s 12,000 is the least trustworthy of the five. Bitset containers and
array-or-run containers are strongly correlated across these principals and only `year300` has a
bitset count out of proportion to its array count (87,062 against 1,973,023), so one principal
carries that rate. It is reported as fitted rather than as measured per container.

## What was run, and what it cost

| | wall |
|---|---|
| release build | 1 m 09 s |
| `gbif-64p-terms` rebuilt at format 14 | 99 s of stages |
| its route probe | 3 m |
| its fold | 60 s |
| **the rung 6 build** | **3 h 46 m 42 s** |
| its route probe, including a 99.7 s open | **18 m** |

Files added by this part: `build-gbif-64p-terms-f14.log`, `serve-gbif-64p-terms-f14.log`,
`build-rung6-f14.log`, `gbif-64p-terms-f14/` and `gbif-terms/` (each a `results.json` and a
`route_probe.log`), and `results.json` beside this file, which carries every figure above.
