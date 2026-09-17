# Term images at rung 6

Status: Part A only, 2026-09-17. The corpus is prepared and the 64-part rung is built and
measured; rung 6 itself is modelled from those measurements and from earlier rung 6 runs, and is
not built. Not normative; re-take a figure before relying on it.

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
