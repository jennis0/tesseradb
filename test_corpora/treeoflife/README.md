# TreeOfLife-200M — the ladder's largest rung, and its first with two geometries over one entity space

**233,055,986 specimen photographs**, each with a 768-dimensional BioCLIP-2 embedding a third party
computed and released on Hugging Face, carrying seven taxonomic ranks, a scientific name, a common
name and — the rung's compartment — **a publishing institution**. 2.3× rung 4's rows at three
quarters of the width, **two views of one entity space**, and the ladder's first seven-level tiered
layer.

It is a **demonstrator and a speed benchmark** (owner rulings 2026-09-01 and 2026-09-02: this
corpus tests Tessera's speed and memory, not the UMAP pipeline; layout quality matters only as far
as the demo looks good). Recall against an exact neighbour search is not measured and layout
fidelity is not judged.

**Three things make this rung different from the four below it.**

1. **Two views over one entity space.** `bioclip` is the embedding layout and holds every row;
   `geo` is Web Mercator over the GBIF-joined rows and holds 75.90% of them. The arXiv rung had two
   views of one embedding and the geographic rungs had one geography each; nothing on the ladder
   has had an embedding *and* a geography over the same entities, or a view holding a subset of the
   corpus.
2. **A seven-level tiered layer over every row**, which is the largest tiered membership the ladder
   has carried and is drawn on both views.
3. **No prose.** The search surface is names — an indexed `scientific_name` keyword and an indexed
   `common_name` text column. Rung 4 measured what 118.9 GB of abstracts do to a build; this rung
   measures what 2.33×10⁸ rows do to one without them.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder

# the join track's two passes over the share — see README-join.md
~/venvs/projection/bin/python -m test_corpora.treeoflife.stage --scan --combine
~/venvs/projection/bin/python -m test_corpora.treeoflife.gbif_join

# the vectors track: the fit sample, then the corpus
~/venvs/projection/bin/python -m test_corpora.treeoflife.stage --fit
~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --layout-only
~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --reuse-layout

cd "$TESSERA_LADDER/treeoflife" && tessera check --payloads && tessera build --stage-timings
```

## Two tracks, one package

`sources.py`, `stage.py`, `gbif_join.py` and [`README-join.md`](README-join.md) are the **join**
track's: the one pass over the 666 source files for every column but `emb`, and the scan of GBIF's
8,369 occurrence parts that gives the `geo` view its coordinates. `routes.py`, `prepare.py`,
`spread.py`, `drive.py`, `corpus.toml` and this file are the **vectors** track's. The interface
between them is `staging/metadata.parquet` and `staging/gbif-coordinates.parquet`, and
`entity_id` is the global row index in file order on both sides.

## The vectors are never staged whole, and that is a disk fact

346 GB of `float16` against **217 GB free** on this box. Rung 4 staged its 209 GB matrix and placed
against a local memmap; here the layout is fitted on a sample staged locally and every row is
placed in a **second pass that reads the share and keeps nothing but a position**.

**The fit sample is uniform over all 666 files, because the corpus is sorted by taxonomy.** A
prefix of the files is a prefix of the tree of life. Each file contributes its equal share, drawn
from **one of its seven row groups**, the group index rotating with the file (`i % 7`).

⊘ **That is one seventh of a pass rather than a whole one, and it is a deliberate trade.** A truly
uniform draw within a file would touch nearly every data page of that file's `emb` column — the
column is 73 MB a row group and a scattered 3,754-row take decodes almost all of it — so a whole
pass would cost hours to sample what a rotating single group samples in half of one. What is given
up is intra-file uniformity, over a file spanning a very narrow taxonomic range.

## The compartment is real, and every row carries a term

`publisher` — the institution that published the occurrence record. **Owner ruling, 2026-09-03:** a
row with no publisher carries `unpublished`, a key of the same closed vocabulary, rather than no
term and the view's `public` default. So the access column is never empty, `point_visibility`'s
`default` never fires, and **a principal holding no term sees nothing**. Written this way because
the campaign's principal ladder starts at 1% of the corpus and cannot be composed under a floor
every principal holds for free. Rung 4's `unlicensed` and rung 3's `unindexed` have the same shape
and exist for the same reason.

⊘ The interface document fixed on 2026-09-03 said `default = "public"`; the ruling later that day
replaced it, and the declaration follows the ruling.

## The taxonomy layer's keys

A member row is one specimen with a seven-entry list whose positions are the declared levels —
GeoNames' `members-admin.parquet` form. Level *j*'s key is the first *j* + 1 ranks joined with `|`,
so a species key names its whole lineage: `species` in this source is the epithet (`lotor`, not
`Procyon lotor`), and a bare epithet is not unique across the tree.

**A hole in the chain becomes an artifact, not a null.** A row with a family and no order carries
an explicit `NOT_RECORDED` key at the order level, because `parent_edges` is `windows(2)` and does
not read past a gap; a null there would state a containment no row makes. A level is filled only
where something *below* it is present, so a chain that simply ends keeps its remaining levels null.
Both markers are checked against the corpus's own values and a collision is a refusal: a
placeholder that merged with a real clade would move specimens between artifacts with no error.

## Ruled 2026-09-03 — no contiguity experiment

The plan's "ingest in taxonomy order and shuffled" is withdrawn. Allocation orders entity ids
within a signature group by the item's Morton code and not by arrival
([`../../docs/design/annotation-representation.md`](../../docs/design/annotation-representation.md)
§2.2: `(signature, morton_code, source_id)`), so input order changes nothing and the two arms would
differ only by the cost of shuffling the input. The rung is built once, with `publisher` as the
compartment. ⊘ The ruling named `docs/decisions/0073-entity-ties-are-ordered-by-morton-code.md`,
which the 2026-09-02 docs cut deleted; the rule it settled is the design document's.

## Measured

All figures **local NVMe on this box** (WSL2, 12 cores, 47 GB, one RTX 3080) unless the medium says
otherwise. The two staging passes are the **network-source** figures.

### Staging — the fit sample, off the share

**2,500,000 rows in 33.9 minutes at 25.0 MB/s**, three concurrent readers, 3.84 GB of
`staging/fit.f16`, zero zero-norm vectors. That is 50.8 GB read — one row group of each of the 666
files — against the 344 GB a whole pass would have been. The join track's own two passes
(`metadata.parquet` in 190 s from local shards after a 666-file scan, and the 8,369-part GBIF scan
at 11.53 MB/s) are in [`README-join.md`](README-join.md).

### The route — fit on 2,500,000, place 233,055,986 off the share

| | |
|---|---|
| CAGRA index over the fit set | **26.5 s** |
| its own kNN graph, 2,500,000 queries | **17.5 s** (142,994 /s) |
| cuML UMAP over that graph | **45.3 s** |
| **place 233,055,986 rows** | **10,505 s — 2 h 55 min**, 20,977 rows/s, **32.2 MB/s** off the share |
| the whole route | 10,536 s at **9.8 GB** peak `VmHWM` |

**The share is the constraint and the card is idle most of the pass.** 344 GB at 32 MB/s is the
2 h 55 min; the GPU search inside it is ~45 minutes at rung 4's measured rate. Three reader threads
is what this SMB mount sustains (20 MB/s on one, ~30 MB/s on two or more, measured 2026-09-03).

⊘ **The pass dropped a read and was made to survive one.** A first attempt died 25 minutes in on
`ZSTD decompression failed: Src size is incorrect`; the positions now go into a memmap with a
per-row-group ledger and a read is retried four times, so a rerun costs the share only what is
missing. **Three transient retries** succeeded over the finished pass.

⊘ **One row group of the source is corrupt, and 50,000 rows carry a position that is not theirs.**
`train-00035-of-00666.parquet` row group 4 fails on its `emb` column at every attempt — from the
share and from a byte copy on local disk — while the same group's `uuid` and rank columns read
cleanly. It is the publisher's bytes, not this box's SMB. 50,000 of 233,055,986 is **0.021%**; the
rule for an input is ignore and report, so those rows take the layout's centroid, `manifest.json`
names the file and the group, and re-acquiring that one file would fix them.

⊘ **The layout is not reproducible under a seed**, and a *resumed* placement is coherent only
because `fit-xy.npy` is written before the placement starts and reused: CAGRA's index build takes
none, so re-fitting after a break would place the remaining groups into a different 2D space.

### The 1,000,000-row sample

`prepare.py --sample 1000000` **111.8 s** at **9.24 GB** peak: route 33.9 s (CAGRA 9.4 s, 1,000,000
queries 6.4 s, UMAP 14.2 s), the GBIF coordinate load 34.1 s, the one streaming pass 40.8 s,
k-means 0.6 s. 17 cells (29,611 … 90,855, median 61,967), 17 of 17 titled out of 16,085 candidate
terms. 760,259 of 1,000,000 (**76.03%**) rows carry a coordinate.

⊘ **The sample is drawn from the staged fit rows, not from the corpus.** Every other row's vector
is on the share, and a scattered 10⁶-row draw would touch nearly every data page of all 666 files —
a three-hour pass to sample what the 2,500,000-row staged file already holds uniformly over them.

`tessera build` **18.4 s** to a **162.3 MB** bundle over 377 terms; `verify --deep` clean in
**0.13 s at 67.7 MB**. The build's memory split, by the text-peak probe's method:

| | anonymous | file-backed | `VmHWM` |
|---|---|---|---|
| whole run high-water | **341 MB** | 172 MB | 508 MB |
| the stage it lands on | `segment_write` | `record_blob` | `record_blob` |

**Served on 8141, both views, and the ladder is what the ruling asked for.** 0 / 583,117 /
1,000,000 on `bioclip` and 0 / 546,396 / 760,259 on `geo`, across *no terms*, `iNaturalist.org` and
all 376 keys. `match common_name:"butterfly"` moves the matched count without moving the visible
one — 1,230 of 1,000,000 — and an absent token matches 0.

### The whole corpus — 233,055,986 specimens

`prepare.py --sample 0 --reuse-layout` **633.7 s — 10.6 minutes** at **9.44 GB** peak, on a box
otherwise idle.

| step | | |
|---|---|---|
| the GBIF coordinates | **13.9 s** | 205,901,893 matched records streamed into two float64 arrays; 176,899,537 carry a coordinate |
| **points, taxonomy and the geo view** | **373.7 s** | the one pass: 233,055,986 seven-entry taxonomy keys built in Arrow, 6.93 GB of `points.parquet` and 2.04 GB of `points-geo.parquet` written |
| k-means | 157.0 s | cuML over the 233M × 2 layout, **whole** rather than sampled, k = 256 |
| vectorise names | 28.8 s | 57,479 candidate terms over a 4,000,000-name sample |
| titles | 5.8 s | 231 of 256 cells titled |
| the clustering's member rows | 17.9 s | 233,055,986, plus 62,484 ranked generating-set rows |

256 k-means cells hold 1 … 5,803,417 specimens (median 149,411).

**What the corpus turned out to be:**

| | |
|---|---|
| with a scientific name | 233,055,986 (**100%**) |
| with a common name | 164,268,064 (**70.5%**) |
| **in the `geo` view** | **176,899,537 (75.90%)** — of 205,901,893 GBIF records matched, the rest carrying no coordinate |
| vocabulary sizes | kingdom 191 · phylum 1,265 · class 4,169 · order 8,357 · family 27,166 · genus 138,171 · species 240,084 · publisher 474 · source dataset 4 · basis of record 8 · image type 13 |
| taxonomy member rows | 233,055,986, one per specimen, seven entries each — **1.63×10⁹ membership entries** |

**The principal ladder** — the ruling's whole point, and the figure every count below is against:

| principal | terms | `bioclip` | `geo` |
|---|---|---|---|
| no terms | — | **0** | **0** |
| `iNaturalist.org` | 1 of 474 | **134,852,438** (57.9%) | **126,390,401** (71.4% of the view) |
| all publishers | 474 of 474 | **233,055,986** | **176,899,537** |

**The two views hold different populations, and that is the rung.** A principal's count differs
between them by the join rate rather than by the mask: `iNaturalist.org` sees 57.9% of the specimen
map and 71.4% of the world map, because its records are more likely than the corpus's average to
carry a coordinate. The rest of the roster, for composing a ladder between those: `observation.org`
21,772,173 · `unpublished` 10,395,236 · `MNHN - Museum national d'Histoire naturelle` 6,118,594 ·
`Naturalis Biodiversity Center` 5,211,080. `branch-ranks.json` beside the corpus carries the whole
list, ranked, which is what a measurement driver composes principals from.

⊘ **No `branch-terms.txt`.** The other rungs write the same roster a second time as one
comma-separated line; **70 of the 474 publisher names contain a comma**, so that file would split
into 544 terms nobody holds. `branch-ranks.json` is JSON and is what the drivers read.

### The build — 70 minutes to a 40 GB bundle, and it converges

`tessera build --stage-timings` **4,233 s — 1 h 10 m 33 s** over twenty stages, **39.97 GB** on
disk, 1,001,193 artifacts minted, 34 unclustered member rows. `verify --deep` clean in **37.1 s at
10.29 GB**.

**The 34 unclustered rows are the corpus, not a defect**: 34 specimens carry no kingdom and nothing
below it, so their whole seven-entry path is null and they are in no artifact at any level.

| stage | wall | `VmHWM` |
|---|---|---|
| `source_ids` | 15.6 s | 4,819 MiB |
| `dictionary` | 388.1 s | 4,819 MiB |
| `geometry_read` | 214.9 s | 13,687 MiB |
| `signature_sort` … `external_ids` | 89.5 s | 18,873 MiB |
| `attribute_tail` | **449.8 s** | 22,560 MiB |
| `layers` | **288.1 s** | 26,020 MiB |
| `text_index` | 32.0 s | 36,101 MiB |
| **`filter_postings`** | **2,125.1 s** | 36,101 MiB |
| `record_blob` | 47.7 s | 36,101 MiB |
| `tiler_sort` + `segment_write`, both views | 407.1 s | 36,101 MiB |
| `manifests` | 172.6 s | 36,101 MiB |

**Half the build is `filter_postings`** — 2,125 s of 4,233 — and that is the price of an indexed
`publisher` column with 474 keys over 2.33×10⁸ rows plus two indexed name columns. Rung 4's build
met its wall in `text_index` on 118.9 GB of abstracts; this rung's `text_index` is **32 seconds**,
because 164 million common names are 2.3 GB of characters and not 119 GB.

**The memory split**, by the text-peak probe's method:

| | anonymous | file-backed | `VmHWM` |
|---|---|---|---|
| whole run high-water | **16,263 MB** | 21,669 MB | **36,101 MB** |
| the stage it lands on | `text_index` | `text_index` | `text_index` |

Two thirds of the resident peak is page cache the kernel may evict; the build's own heap high-water
is **15.9 GB on a 47 GB box**, and it reaches it at the stage where the layers' spill and the text
index's runs are both mapped.

**Bundle breakdown**, 39.97 GB:

| | |
|---|---|
| `views` | **19 GB** — `bioclip` 11 GB, `geo` 8.1 GB |
| `attrs` | **12 GB** — of which `uuid` **8.0 GB**, the record blob 1.6 GB, `scientific_name` 927 MB, `common_name` 639 MB, `publisher` 473 MB, `source_dataset` 253 MB |
| `row-column` | 3.2 GB |
| `members` | 2.3 GB |
| `entities` | 1.8 GB |
| `MANIFEST.json` | 44 MB |
| `containment`, `terms`, `tile-index`, `dictionary` | 9 MB together |

**A 36-byte indexed keyword is a fifth of the bundle.** `uuid` is 8.0 GB of 40 GB — more than both
name columns and the record blob together — and it buys a lookup nobody on this map performs. It is
declared because the interface asked for it; a rung that wanted the bundle smaller would drop it
first.

⊘ **Both views report RESOLUTION LOST, and only one of them is news.** `bioclip` places
233,055,986 points in 8,330,745 distinct cells — **3.6%** have a position of their own — and the
frame is already `extent = "auto"`. The cause is fit-and-place at a 93:1 ratio: a row is placed at
the similarity-weighted mean of its fifteen fit-set neighbours, so rows sharing those neighbours
share a position exactly, and the corpus carries very large near-duplicate sets. `geo` places
176,899,537 points in 12,002,377 cells (**6.8%**) against Web Mercator's whole domain, where a
16-bit cell is ~600 m and specimens genuinely coincide at a locality. **Neither is a defect of the
build**: every position written is correct, only coarse. The `bioclip` figure is a property of the
route and is the price of not staging 346 GB.

### Served, and a 24 GiB cap does not break it

`tessera serve` on 8141 against the 39.97 GB bundle. **Open costs 87.7 s** building every level's
artifact row form over 1,001,193 artifacts across two views, and leaves **15.96 GB anonymous**
resident before any request — the per-process floor rung 3 first measured, here over a membership
26× larger than that rung's clustering. Driven by `drive.py`: three principals × a 25-request pan
over five zoom levels **per view**, a `match` on `common_name`, and thirty drill-downs.

| request kind | n | uncapped p50 | uncapped p99 | 24 GiB p50 | 24 GiB p99 |
|---|---|---|---|---|---|
| `bioclip` pan, *no terms* | 25 | 0.07 ms | 40.8 ms | 0.06 ms | 2.0 ms |
| `bioclip` pan, `iNaturalist.org` | 25 | 0.11 ms | 1,932 ms | 0.10 ms | 1,512 ms |
| `bioclip` pan, all publishers | 25 | 0.16 ms | 1,078 ms | 0.13 ms | 1,088 ms |
| `geo` pan, *no terms* | 25 | 0.16 ms | 103.6 ms | 0.11 ms | 2.1 ms |
| `geo` pan, `iNaturalist.org` | 25 | 1.03 ms | 1,364 ms | 1.08 ms | 1,126 ms |
| `geo` pan, all publishers | 25 | 0.89 ms | 1,073 ms | 0.74 ms | 943 ms |
| `match common_name:"butterfly"` | 5 | 28.1 ms | 42.7 ms | 28.1 ms | 58.0 ms |
| `match` an absent token | 5 | 9.9 ms | 11.8 ms | 9.8 ms | 10.6 ms |

**It survives the cap.** `systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0`: `oom 0`,
`oom_kill 0`, `memory.peak` sits exactly at the cap with 3,449 reclaim-at-max events over the drive
(19,942 over the longer battery), and **160 of 160 count-bearing responses are identical between
the capped and uncapped runs**. Uncapped, the scope's `memory.peak` is 27.4 GB.

**Counts move with the mask, on a built bundle, in both views**: 0 / 134,852,438 / 233,055,986 on
`bioclip` and 0 / 126,390,401 / 176,899,537 on `geo`. `match common_name:"butterfly"` matches
**256,097** of 233,055,986 without moving the visible count; an absent token matches 0.

### The campaign battery — the principal ladder, hot and cold

`serve_battery.py` on `bioclip`, reduced to zooms 0/6/12, decile 9, one cell per decile, 40 hot
samples and 6 cold, uncapped and under 24 GiB. Both runs clean: `oom_kill` false, zero request
failures.

| target | measured | terms | first viewport | zoom-0 hot p50 | zoom-0 cold p50 |
|---|---|---|---|---|---|
| 1% | 1.00% | 6 | 1.65 s | 94.7 ms | 2.00 s |
| 5% | 5.00% | 5 | 2.39 s | 208.7 ms | 2.25 s |
| 10% | 10.00% | 6 | 4.65 s | 265.6 ms | 2.77 s |
| 25% | 25.00% | 12 | 4.59 s | 501.7 ms | 3.65 s |
| 50% | **42.14%** | 473 | 6.10 s | 750.4 ms | 5.32 s |
| 100% | 100.00% | 474 | **28.23 s** | 1,315.3 ms | **27.1 s** |

⊘ **The 50% target cannot be hit and the reason is the compartment's shape.** `iNaturalist.org`
alone is 57.9% of the corpus, so a greedy composition under a 50% budget takes every *other*
publisher — 473 terms — and reaches 42.14%. The ladder's other five targets are exact.

**The cap costs almost nothing hot and helps cold.** At the 100% principal, zoom-0 hot p50 moves
1,315 → 1,339 ms and cold 27.1 → 26.2 s; at zooms 6 and 12 the capped run's
`cold_pages_warm_engine` is **three to five times faster** (20 ms against 55 ms), which is the
eviction the scope makes possible rather than an improvement.

**Zoom 0 is the expensive request and the tiered layer is why.** A whole-extent viewport at the
100% principal carries the artifact frame for a layer with 1,001,193 artifacts; the campaign's
first finding (§6 of `docs/ingest-campaign.md`, at 464,655 artifacts) is here at 2.2× the size.

### Layer spread — the two views disagree, and that is the finding

The box between the 5th and 95th percentile of an artifact's members, as a share of the view's box.
A uniform sample of 1,014 taxonomy keys across the seven levels, and every publisher.

| layer | view | median | p90 | max | under 5% |
|---|---|---|---|---|---|
| `taxonomy/tree` (662 measured) | `bioclip` | **0.0205%** | 0.081% | 0.175% | **100%** |
| `taxonomy/tree` (463 measured) | `geo` | **1.71%** | 37.9% | 51.5% | 63% |
| `publishers/source` (449) | `bioclip` | **0.064%** | 0.099% | 0.706% | **100%** |
| `publishers/source` (402) | `geo` | **0.169%** | 16.1% | 62.3% | 83% |

*by level, `taxonomy/tree`:*

| | kingdom | phylum | class | order | family | genus | species |
|---|---|---|---|---|---|---|---|
| `bioclip` | 0.039% | 0.037% | 0.020% | 0.013% | 0.015% | 0.022% | 0.016% |
| `geo` | 37.8% | 15.9% | 4.70% | 6.32% | 4.73% | 0.711% | **0.118%** |

**A clade is compact in the embedding at every level and only compact in geography once you are
below family.** On `geo` the tree tightens by two and a half orders of magnitude from kingdom to
species, which is the shape a boundary layer has and the shape the zoom→level map is declared for.
On `bioclip` even a kingdom is 0.04% of the frame — but that frame is set by a handful of outlying
fit positions, so every share on that view is against a mostly empty box and the *ratios* between
levels are the readable part, not the absolute figures.

**A publisher is the mirror image**: spread through the tree and compact on the ground (median
0.17% of the world), because an institution collects near itself and across the tree of life. That
is the first time a rung's two layers have separated on two geometries of one entity space.

⊘ **The build's `everywhere` fraction disagrees with all of this, and the two measure different
things** — rung 3's and rung 4's finding again. `taxonomy/tree` reports 0.51–0.69 everywhere at
80–175 blocks an artifact on `bioclip`; an artifact whose members occupy 0.02% of the map is still
spread across enough of row space that no tile-index node bounds it. Compactness in the map and
boundability in row space are different properties.

**Neither layer is withdrawn.** Against the taxonomies withdrawn at rungs 1 and 2 (medians 13.7%
and 9.6%) both draw on both views.


### The ingest cycle at f = 50% — 116.5M rows in, and one finding that is the driver's

`ingest_cycle.py --fraction 0.50 --concurrency 8 --state-extent`, 4 h 2 m end to end. The base is
the complement's **points and declarations alone**; every artifact is meant to arrive on the wire
afterwards.

| | |
|---|---|
| the split | 116,527,993 base rows, 116,527,993 held back |
| base build (points and declarations only) | 25 min to a 19 GB bundle; opens in 12.6 s |
| **the hold-out, ingested** | **116,527,993 accepted in 10,536 s — 11,060 items/s** at *C* = 8 |
| ack latency | p50 **5.99 s**, p99 **15.13 s**, max 48.6 s |
| statuses | 11,653 × 200, and 2,487 × 429 retried — backpressure, not error |
| flush | ⊘ the executor's counter did not move within the driver's 900 s timeout |
| **fold** | **1,313 s** (the server's own `compaction.last_secs`) at **28.9 GB**, 1 fold, 0 failures |
| after the fold | `verify --deep` clean: **321,511,824 rows** over two views, `entity_id_high_water` 233,055,986, **233,055,986 external-id bindings** |

**Nothing is lost, and the served-side arithmetic closes exactly.** The folded deployment holds
**233,055,986 `bioclip` rows** — every base row and every hold-out row — and **88,455,838 `geo`
rows**, which are the base's own: the driver's wire batch carries one row space, so the hold-out
enters the **anchor view alone**. A rung with several row spaces measures the write path on one of
them, and a `geo`-side census differs by construction rather than by defect.

⊘ **2,142,399 rows are visible on the all-in bundle and not on the folded one, and the cause is the
driver's wire encoding.** `ingest_cycle.encode_batch` writes the passthrough plugin's `access` as a
**comma-separated descriptor list**, and **70 of the 474 publisher names contain a comma**. On the
wire each splits into fragments, and every fragment that is not already a term is minted: the folded
deployment carries **617 terms against the declaration's 475**. Measured on it directly:

| principal | `bioclip` visible |
|---|---|
| the 474 declared publisher terms | **230,913,587** |
| those plus the 148 wire fragments | **233,055,986** |

2,217,001 hold-out rows carry a comma name; **74,602 stay visible** because a fragment of their name
is itself a declared key — 73,442 of them `Natural History Museum, Vienna`, whose first fragment is
the real publisher `Natural History Museum` — and the other **2,142,399 are invisible to every
declared principal**. **Where a fragment is a real key the rows land in that compartment instead**,
which is why the 25% principal sees **73,212 rows more** on the folded deployment than on the all-in
one. It is the driver's encoding and not the build: the all-in bundle keys the same rows correctly,
and this is the first rung whose compartment keys contain the separator that encoding uses. It is
also why this rung writes no `branch-terms.txt`.

**The equivalence census, retaken.** The run's own was shed mid-body — `ChunkedEncodingError`, on
the zoom-0 whole-extent request with `layers: "all"` over 1,001,193 artifacts against a stream
deadline, which is the post-flush artifact-frames finding at this scale and not a count difference.
Retaken once after the fold with both sides served in turn, it went through:

| principal | folded | all-in | difference |
|---|---|---|---|
| 1% | 2,309,136 | 2,309,136 | **exact** |
| 5% | 11,545,679 | 11,545,679 | **exact** |
| 10% | 23,091,359 | 23,091,359 | **exact** |
| 25% | 57,801,609 | 57,728,397 | +73,212 |
| 50% | 96,061,149 | 98,203,548 | −2,142,399 |
| 100% | 230,913,587 | 233,055,986 | −2,142,399 |

22 differences over three surfaces: 3 at zoom 0, 6 at box level, 13 on layers. **Every layer
difference is a layer that was never published.** `clusters/kmeans` is declined at 233,118,470
member rows — the driver inverts a layer's whole membership in memory to address it per artifact —
and `taxonomy/tree` is not in the driver's layer roster at all, its member file being list-keyed,
which the publication path does not take. Both are therefore declared and empty on the folded
deployment. **`publishers/source` needs no publication** — its membership is the indexed column —
and it reproduces exactly at five of the six principals, the sixth being the wire-encoding
difference above.

### What is on disk

`$TESSERA_LADDER/treeoflife` is **78 GB**, `treeoflife-1m` beside it 221 MB, and the measurement
work under `$TESSERA_LADDER/.measure/treeoflife` **40 GB** — almost all of it the ingest cell's base
bundle and its folded copy, which is deletable once the cell's JSON is committed.

| | |
|---|---|
| `bundle/` | **38 GB** |
| `staging/` | **30 GB** — `metadata.parquet` 8.3 GB and its 666 shards 8.3 GB, the GBIF join's parts and ids 7.4 GB, `gbif-coordinates.parquet` 2.4 GB, the placement checkpoint 1.8 GB, **`fit.f16` 3.6 GB** |
| `points.parquet` | 6.5 GB |
| `points-geo.parquet` | 1.9 GB |
| `layout-bioclip.npy` | 1.8 GB — the whole corpus's positions, so a rebuild needs no GPU and no share pass |
| the two member files | 696 MB |

**The 346 GB of vectors are not here and never were.** A run that wants a *new* layout re-reads the
share, which is ~3.5 hours; a run that wants a new corpus over the same layout reads
`layout-bioclip.npy` and costs 10.6 minutes. `staging/placement/` is the resumable checkpoint —
`place.f32` and its ledger — and can go once the layout is written.

## The environment

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU with scikit-learn on the CPU — shared with
the arXiv, MedCPT and PaperSeek rungs; `requirements.txt` points at the arXiv rung's. `drive.py` is
the one exception and runs on the system `python3`: it needs `requests` and `pyarrow` and nothing
the rung's environment carries.
